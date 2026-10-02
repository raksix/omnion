"use client";

/**
 * `/automation/projects` — the project list (docs/requests/REQ-133, slice 1).
 *
 * The screen is a container list, and every choice below follows from that it is a *bucket*
 * rather than a record:
 *
 * 1. **Key and name are both shown, and they are not the same thing.** The key is what a person
 *    types into a ticket ("PLAT") and the name is what the team calls it. A list showing only
 *    the name makes the key undiscoverable, which defeats the reason it exists.
 * 2. **The default project is marked, and marked as protected.** Not as a nicety: it is where
 *    every resource created without an explicit project lands, so archiving it would break
 *    every insert path that does not pass one. The row says so, and the archive button is not
 *    offered — asking the reader to confirm an action the server will refuse is a lie about
 *    what the button does.
 * 3. **An archived project reads as read-only rather than broken.** Its row stays, dimmed, with
 *    a restore action. A greyed-out row with no way back looks like a bug, and it is the state
 *    an operator most needs to escape.
 * 4. **The filter lives in the URL**, so a filtered list is a link a colleague can open and the
 *    selection survives a reload instead of quietly resetting to "everything".
 * 5. **The two empty states are different states.** "No projects yet" and "the filter hid them"
 *    are different situations with different buttons, and a single dead end answers neither —
 *    the same rule the lead inbox follows.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useRouter, useSearchParams } from "next/navigation";
import { Archive, ArchiveRestore, FolderPlus, Loader2, RefreshCw, Search, TriangleAlert } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError, createProject, fetchProjects, setProjectArchived } from "@/lib/api";
import type { Project } from "@/lib/types";

const STATUS_LABEL: Record<string, string> = {
  active: "Active",
  archived: "Archived",
};

/** Read the filters out of the query string. */
function filtersFrom(params: URLSearchParams): { q: string; status: string } {
  const status = params.get("status") ?? "";
  return { q: params.get("q") ?? "", status: status === "archived" ? "archived" : "" };
}

export function ProjectList() {
  const router = useRouter();
  const params = useSearchParams();
  const filters = useMemo(
    () => filtersFrom(new URLSearchParams(params?.toString() ?? "")),
    [params],
  );

  const [projects, setProjects] = useState<Project[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [search, setSearch] = useState(filters.q);
  const [creating, setCreating] = useState(false);
  const [busyId, setBusyId] = useState<string | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      setProjects(await fetchProjects());
    } catch (cause) {
      setProjects([]);
      setError(cause instanceof ApiError ? cause.message : "the projects could not be read");
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  // The search box is the URL's copy while the reader types in it: local state makes it
  // responsive, the debounced write is what makes a filtered list shareable.
  useEffect(() => {
    if (search === filters.q) return;
    const timer = setTimeout(() => {
      const next = new URLSearchParams(params?.toString() ?? "");
      if (search) next.set("q", search);
      else next.delete("q");
      router.replace(next.toString() ? `?${next.toString()}` : "?", { scroll: false });
    }, 250);
    return () => clearTimeout(timer);
  }, [search, filters.q, params, router]);

  // Filtering happens over the rows the API already returned and over nothing else. The list is
  // scoped server-side; a client-side count over a scoped list is the count that disagrees with
  // the API's.
  const visible = useMemo(() => {
    if (!projects) return [];
    const needle = filters.q.trim().toLowerCase();
    return projects.filter((project) => {
      if (filters.status && project.status !== filters.status) return false;
      if (!needle) return true;
      return (
        project.key.toLowerCase().includes(needle) ||
        project.name.toLowerCase().includes(needle) ||
        project.description.toLowerCase().includes(needle)
      );
    });
  }, [projects, filters.q, filters.status]);

  const setStatus = (status: string) => {
    const next = new URLSearchParams(params?.toString() ?? "");
    if (status) next.set("status", status);
    else next.delete("status");
    router.replace(next.toString() ? `?${next.toString()}` : "?", { scroll: false });
  };

  const archive = async (project: Project) => {
    const archived = project.status !== "archived";
    if (project.is_default) return;
    const question = archived
      ? `Archive ${project.name}? New work stops; the history stays and a restore brings it back.`
      : `Restore ${project.name}? Its schedules and history are unchanged.`;
    if (!window.confirm(question)) return;
    setBusyId(project.id);
    try {
      await setProjectArchived(project.id, archived, project.key);
      await load();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "the project could not be changed");
    } finally {
      setBusyId(null);
    }
  };

  const create = async () => {
    const name = window.prompt("Project name");
    if (!name?.trim()) return;
    // The key is asked for rather than derived. A derived key is either the name uppercased —
    // which collides the moment two projects share a word — or a random string, which nobody
    // remembers. The API refuses anything but 2-8 uppercase letters or digits and says so.
    const suggestion = name
      .trim()
      .replace(/[^A-Za-z0-9]/g, "")
      .slice(0, 8)
      .toUpperCase();
    const key = window.prompt(
      `Short key for “${name.trim()}” — 2 to 8 uppercase letters or digits`,
      suggestion,
    );
    if (!key?.trim()) return;
    setCreating(true);
    try {
      const created = await createProject({ key: key.trim().toUpperCase(), name: name.trim() });
      await load();
      router.push(`/automation/projects/${created.id}`);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "the project could not be created");
    } finally {
      setCreating(false);
    }
  };

  const activeFilters = filters.status ? 1 : 0;

  return (
    <div className="flex flex-col gap-3">
      <section className="flex flex-col gap-2.5">
        <div className="flex flex-wrap items-center gap-2">
          <label className="flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px]">
            <Search className="size-3.5 text-muted" aria-hidden />
            <span className="sr-only">Search projects</span>
            <input
              type="search"
              data-project-search
              value={search}
              placeholder="Search key or name"
              className="w-44 bg-transparent outline-none"
              onChange={(event) => setSearch(event.target.value)}
            />
          </label>

          <fieldset className="flex flex-wrap items-center gap-1.5" data-project-status-filter>
            <legend className="sr-only">Filter by status</legend>
            {[
              { value: "", label: "All" },
              { value: "active", label: "Active" },
              { value: "archived", label: "Archived" },
            ].map((option) => {
              const active = filters.status === option.value;
              return (
                <button
                  key={option.value}
                  type="button"
                  aria-pressed={active}
                  data-status={option.value || "all"}
                  onClick={() => setStatus(option.value)}
                  className={`rounded-full border px-2.5 py-1 text-[11.5px] transition ${
                    active
                      ? "border-accent bg-accent-soft text-accent-strong"
                      : "border-line text-ink hover:bg-quiet-soft"
                  }`}
                >
                  {option.label}
                </button>
              );
            })}
          </fieldset>

          <div className="ml-auto flex items-center gap-2">
            {activeFilters > 0 || filters.q ? (
              <button
                type="button"
                data-project-reset
                onClick={() => {
                  setSearch("");
                  setStatus("");
                }}
                className="rounded-lg px-2.5 py-1.5 text-[12.5px] text-accent-strong hover:underline"
              >
                Reset filters
              </button>
            ) : null}
            <button
              type="button"
              data-project-refresh
              onClick={() => void load()}
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] text-muted transition hover:text-ink"
            >
              <RefreshCw className="size-3.5" aria-hidden />
              Refresh
            </button>
            <button
              type="button"
              data-project-create
              disabled={creating}
              onClick={() => void create()}
              className="inline-flex items-center gap-1.5 rounded-lg border border-accent bg-accent px-2.5 py-1.5 text-[12.5px] text-white transition disabled:opacity-60"
            >
              {creating ? (
                <Loader2 className="size-3.5 animate-spin" aria-hidden />
              ) : (
                <FolderPlus className="size-3.5" aria-hidden />
              )}
              New project
            </button>
          </div>
        </div>
      </section>

      {error ? (
        <div
          role="alert"
          data-project-error
          className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]"
        >
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
          <span className="flex-1">{error}</span>
          <button
            type="button"
            onClick={() => void load()}
            className="rounded-lg border border-line px-2 py-1 text-[11.5px]"
          >
            Retry
          </button>
        </div>
      ) : null}

      <div className="overflow-hidden rounded-xl border border-line bg-surface">
        {projects === null ? (
          <LoadingTable columns={6} rows={4} />
        ) : visible.length === 0 ? (
          activeFilters > 0 || filters.q ? (
            <EmptyState
              title="No project matches these filters."
              hint="Every project is still here — the search and the status filter are what hid them."
              action={
                <button
                  type="button"
                  onClick={() => {
                    setSearch("");
                    setStatus("");
                  }}
                  className="rounded-lg border border-line px-2.5 py-1.5 text-[12.5px]"
                >
                  Clear filters
                </button>
              }
            />
          ) : (
            <EmptyState
              title="No projects yet"
              hint="A project is a bucket for one team's automations. Every installation starts with a protected Default project; the ones you add here are yours to shape."
              action={
                <button
                  type="button"
                  data-project-create-empty
                  onClick={() => void create()}
                  className="rounded-lg border border-accent bg-accent px-2.5 py-1.5 text-[12.5px] text-white"
                >
                  Create the first project
                </button>
              }
            />
          )
        ) : (
          <div className="overflow-x-auto">
            <table data-project-table className="w-full border-collapse text-left text-[13px]">
              <caption className="sr-only">
                {visible.length} of {projects.length} projects
              </caption>
              <thead>
                <tr className="border-b border-line text-[11.5px] text-muted">
                  <th scope="col" className="px-3 py-2.5">Key</th>
                  <th scope="col" className="px-3 py-2.5">Name</th>
                  <th scope="col" className="px-3 py-2.5">Members</th>
                  <th scope="col" className="px-3 py-2.5">Workflows</th>
                  <th scope="col" className="px-3 py-2.5">Your role</th>
                  <th scope="col" className="px-3 py-2.5">Status</th>
                  <th scope="col" className="px-3 py-2.5"><span className="sr-only">Actions</span></th>
                </tr>
              </thead>
              <tbody>
                {visible.map((project) => {
                  const archived = project.status === "archived";
                  return (
                    <tr
                      key={project.id}
                      data-project-row={project.key}
                      className={`border-b border-line last:border-b-0 ${archived ? "text-muted" : ""}`}
                    >
                      <td className="px-3 py-2.5">
                        <Link className="text-accent-strong hover:underline" href={`/automation/projects/${project.id}`}>
                          {project.key}
                        </Link>
                        {project.is_default ? <span className="tag tag-muted ml-1.5">Default</span> : null}
                      </td>
                      <td className="px-3 py-2.5">
                        <Link className="hover:underline" href={`/automation/projects/${project.id}`}>
                          {project.name}
                        </Link>
                        {project.description ? (
                          <p className="mt-0.5 max-w-md text-[11.5px] text-muted">{project.description}</p>
                        ) : null}
                      </td>
                      <td className="px-3 py-2.5">{project.member_count}</td>
                      <td className="px-3 py-2.5">{project.workflow_count}</td>
                      <td className="px-3 py-2.5">
                        {project.caller_role ? (
                          <span className="tag">{project.caller_role}</span>
                        ) : (
                          <span className="text-[11.5px] text-muted">Instance administrator</span>
                        )}
                      </td>
                      <td className="px-3 py-2.5">
                        <span className={archived ? "tag tag-muted" : "tag"}>
                          {STATUS_LABEL[project.status] ?? project.status}
                        </span>
                      </td>
                      <td className="px-3 py-2.5 text-right">
                        {project.is_default ? (
                          <span
                            className="text-[11.5px] text-muted"
                            title="Every resource created without a project lands here"
                          >
                            Protected
                          </span>
                        ) : (
                          // One control, not two: an archived row's action is Restore, so the
                          // column never shows an Archive button on a row it would refuse.
                          <button
                            type="button"
                            data-project-archive={project.key}
                            disabled={busyId === project.id}
                            onClick={() => void archive(project)}
                            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:text-ink disabled:opacity-60"
                          >
                            {busyId === project.id ? (
                              <Loader2 className="size-3.5 animate-spin" aria-hidden />
                            ) : archived ? (
                              <ArchiveRestore className="size-3.5" aria-hidden />
                            ) : (
                              <Archive className="size-3.5" aria-hidden />
                            )}
                            {archived ? "Restore" : "Archive"}
                          </button>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}

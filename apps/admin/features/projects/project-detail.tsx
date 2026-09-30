"use client";

/**
 * `/automation/projects/{id}` — one project, its members and its own fields (REQ-133, slice 1).
 *
 * The screen is a detail with a roster, and the two halves have different rules:
 *
 * 1. **A `404` is a not-found screen, not an error about permissions.** The API answers `404`
 *    for a project the caller may not see, on purpose: a `403` would confirm the row exists.
 *    The screen therefore says "no such project" and stops, rather than rendering a permission
 *    message that would undo the answer the API just gave.
 * 2. **The role column explains what each role may do**, because the matrix is a function of
 *    the role rather than a table of strings, and a person deciding who to add is deciding
 *    something the API already knows.
 * 3. **Removing the last owner is refused by the API, and the button explains it beforehand.**
 *    The rule is about the *result* ("at least one owner must remain"), so a delete that
 *    succeeded and then reported success would leave a project nobody can administer. The
 *    refusal is the feature; the screen says which case is which rather than guessing.
 * 4. **An archived project renders read-only with a restore action** — a project that has been
 *    archived is a normal state, and every control being live is what makes it look like a bug.
 */
import { useCallback, useEffect, useState } from "react";
import Link from "next/link";
import { useParams } from "next/navigation";
import { ArrowLeft, Archive, ArchiveRestore, ArrowRightLeft, FolderInput, Loader2, Save, Trash2, TriangleAlert, UserPlus } from "lucide-react";

import { LoadingTable } from "@/components/loading-table";
import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  fetchProject,
  fetchWorkflowsInProject,
  removeProjectMember,
  setProjectArchived,
  setProjectMember,
  updateProject,
} from "@/lib/api";
import { MoveDialog } from "@/features/workflows/move-dialog";
import { TransferOwnershipDialog } from "@/features/projects/transfer-ownership-dialog";
import type { Project, ProjectMember, ProjectRole, Workflow } from "@/lib/types";

/** What each role may do, in the words the members screen needs. */
const ROLE_ABILITY: Record<ProjectRole, string> = {
  owner: "Everything inside the project, including its members and its lifecycle",
  editor: "Create and edit workflows and credentials, and run them",
  operator: "Start, retry and cancel runs; does not change definitions",
  viewer: "Read only",
};

const ROLES: ProjectRole[] = ["owner", "editor", "operator", "viewer"];

export function ProjectDetail() {
  const params = useParams<{ id: string }>();
  const projectId = params?.id;

  const [project, setProject] = useState<Project | null>(null);
  const [members, setMembers] = useState<ProjectMember[]>([]);
  const [missing, setMissing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [rowError, setRowError] = useState<string | null>(null);

  const [key, setKey] = useState("");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [saved, setSaved] = useState(false);

  // The workflows this project holds, and which one the move dialog is open for. Both are here
  // rather than on a workflows screen because this branch has none: the API is scoped by project
  // (REQ-133 slice 2) but the panel has no list to hang the action on, and an action with no host
  // is an action nobody can reach.
  const [workflows, setWorkflows] = useState<Workflow[]>([]);
  const [workflowsError, setWorkflowsError] = useState<string | null>(null);
  const [moving, setMoving] = useState<Workflow | null>(null);
  const [handingOver, setHandingOver] = useState(false);

  const load = useCallback(async () => {
    if (!projectId) return;
    setError(null);
    setMissing(false);
    try {
      const body = await fetchProject(projectId);
      setProject(body.project);
      setMembers(body.members);
      // The workflow list is a separate read: a project whose detail loads while its workflow
      // list fails must still be editable, so this one catches into its own strip rather than
      // failing the screen.
      setWorkflowsError(null);
      try {
        const listed = await fetchWorkflowsInProject(projectId);
        setWorkflows(listed.workflows);
      } catch (cause) {
        setWorkflowsError(
          cause instanceof ApiError ? cause.message : "the workflows could not be read",
        );
      }
      setKey(body.project.key);
      setName(body.project.name);
      setDescription(body.project.description);
    } catch (cause) {
      if (cause instanceof ApiError && cause.status === 404) {
        setMissing(true);
        return;
      }
      setError(cause instanceof ApiError ? cause.message : "the project could not be read");
    }
  }, [projectId]);

  useEffect(() => {
    void load();
  }, [load]);

  const save = async () => {
    if (!project) return;
    setBusy(true);
    setError(null);
    setSaved(false);
    try {
      await updateProject(project.id, {
        key: key.trim().toUpperCase(),
        name: name.trim(),
        description,
      });
      await load();
      setSaved(true);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "the project could not be saved");
    } finally {
      setBusy(false);
    }
  };

  const toggleArchived = async () => {
    if (!project) return;
    const archived = project.status !== "archived";
    if (archived && !window.confirm(`Archive ${project.name}? New work stops; the history stays.`)) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await setProjectArchived(project.id, archived, project.key);
      await load();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "the project could not be changed");
    } finally {
      setBusy(false);
    }
  };

  const addMember = async () => {
    if (!project) return;
    const userId = window.prompt("Account id to add");
    if (!userId?.trim()) return;
    const role = window.prompt("Role — owner, editor, operator or viewer", "viewer");
    if (!role?.trim()) return;
    if (!ROLES.includes(role.trim() as ProjectRole)) {
      setRowError(`“${role.trim()}” is not a project role; it is owner, editor, operator or viewer.`);
      return;
    }
    setBusy(true);
    setRowError(null);
    try {
      await setProjectMember(project.id, userId.trim(), role.trim() as ProjectRole);
      await load();
    } catch (cause) {
      setRowError(cause instanceof ApiError ? cause.message : "the member could not be added");
    } finally {
      setBusy(false);
    }
  };

  const changeRole = async (member: ProjectMember, next: ProjectRole) => {
    if (!project || next === member.role) return;
    setBusy(true);
    setRowError(null);
    try {
      await setProjectMember(project.id, member.user_id, next);
      await load();
    } catch (cause) {
      setRowError(cause instanceof ApiError ? cause.message : "the role could not be changed");
    } finally {
      setBusy(false);
    }
  };

  const remove = async (member: ProjectMember) => {
    if (!project) return;
    if (!window.confirm(`Remove ${member.display_name} from this project?`)) return;
    setBusy(true);
    setRowError(null);
    try {
      await removeProjectMember(project.id, member.user_id);
      await load();
    } catch (cause) {
      setRowError(cause instanceof ApiError ? cause.message : "the member could not be removed");
    } finally {
      setBusy(false);
    }
  };

  if (missing) {
    // The same words the API used, and no more. Saying "you are not allowed" here would
    // confirm the project exists, which is precisely what the 404 was chosen to prevent.
    return (
      <EmptyState
        title="No such project"
        hint="It may have been deleted, or it belongs to a team you are not a member of. Both look the same from here on purpose."
        action={
          <Link
            href="/automation/projects"
            className="rounded-lg border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            Back to projects
          </Link>
        }
      />
    );
  }

  if (error && !project) {
    return (
      <div
        role="alert"
        className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]"
      >
        <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
        <span className="flex-1">{error}</span>
        <button type="button" onClick={() => void load()} className="rounded-lg border border-line px-2 py-1 text-[11.5px]">
          Retry
        </button>
      </div>
    );
  }

  if (!project) {
    return <LoadingTable columns={4} rows={3} />;
  }

  const archived = project.status === "archived";
  const ownerCount = members.filter((member) => member.role === "owner").length;
  const dirty = key !== project.key || name !== project.name || description !== project.description;

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center gap-2">
        <Link href="/automation/projects" className="inline-flex items-center gap-1.5 text-[12.5px] text-muted hover:text-ink">
          <ArrowLeft className="size-3.5" aria-hidden />
          Projects
        </Link>
        <span className="ml-auto flex items-center gap-2">
          {project.is_default ? <span className="tag tag-muted">Default</span> : null}
          <span className={archived ? "tag tag-muted" : "tag"}>{archived ? "Archived" : "Active"}</span>
          {project.caller_role ? <span className="tag">{project.caller_role}</span> : null}
        </span>
      </div>

      {/* The two sub-screens the REQ's route table names, linked rather than reachable only by
          typing a URL. A screen with no link is a screen nobody finds, and a route nobody visits
          is a route the walkthrough cannot judge. */}
      <nav className="flex flex-wrap items-center gap-1.5" aria-label="Project sections">
        <Link
          href={`/automation/projects/${project.id}`}
          data-project-nav="detail"
          aria-current="page"
          className="rounded-lg border border-line px-2.5 py-1 text-[12px]"
        >
          Overview
        </Link>
        <Link
          href={`/automation/projects/${project.id}/limits`}
          data-project-nav="limits"
          className="rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:text-ink"
        >
          Limits &amp; usage
        </Link>
        <Link
          href={`/automation/projects/${project.id}/audit`}
          data-project-nav="audit"
          className="rounded-lg border border-line px-2.5 py-1 text-[12px] transition hover:text-ink"
        >
          Audit
        </Link>
      </nav>

      {error ? (
        <div role="alert" className="flex items-start gap-2 rounded-lg border border-red-500/40 bg-red-500/5 px-3 py-2.5 text-[12.5px]">
          <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-red-600" aria-hidden />
          <span className="flex-1">{error}</span>
        </div>
      ) : null}

      {archived ? (
        <p className="rounded-lg border border-line bg-quiet-soft px-3 py-2.5 text-[12.5px] text-muted">
          This project is archived, so it is read-only. Its history and schedules are unchanged —
          restoring it puts it back into service.
        </p>
      ) : null}

      <section className="flex flex-col gap-3">
        <div className="grid gap-3 sm:grid-cols-[10rem_1fr]">
          <label className="flex flex-col gap-1">
            <span className="text-[11.5px] text-muted">Key</span>
            <input
              value={key}
              data-project-key
              disabled={archived || project.is_default}
              onChange={(event) => setKey(event.target.value.toUpperCase())}
              className="rounded-lg border border-line px-2.5 py-1.5 text-[13px] outline-none focus:border-accent disabled:bg-quiet-soft"
            />
          </label>
          <label className="flex flex-col gap-1">
            <span className="text-[11.5px] text-muted">Name</span>
            <input
              value={name}
              data-project-name
              disabled={archived}
              onChange={(event) => setName(event.target.value)}
              className="rounded-lg border border-line px-2.5 py-1.5 text-[13px] outline-none focus:border-accent disabled:bg-quiet-soft"
            />
          </label>
        </div>
        <label className="flex flex-col gap-1">
          <span className="text-[11.5px] text-muted">Description</span>
          <textarea
            value={description}
            data-project-description
            disabled={archived}
            rows={2}
            onChange={(event) => setDescription(event.target.value)}
            className="rounded-lg border border-line px-2.5 py-1.5 text-[13px] outline-none focus:border-accent disabled:bg-quiet-soft"
          />
        </label>
        <div className="flex items-center gap-2">
          <button
            type="button"
            data-project-save
            disabled={busy || archived || !dirty || !name.trim()}
            onClick={() => void save()}
            className="inline-flex items-center gap-1.5 rounded-lg border border-accent bg-accent px-2.5 py-1.5 text-[12.5px] text-white disabled:opacity-60"
          >
            {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <Save className="size-3.5" aria-hidden />}
            Save
          </button>
          {saved && !dirty ? <span className="text-[11.5px] text-muted">Saved</span> : null}
          {dirty && !archived ? <span className="text-[11.5px] text-muted">Unsaved changes</span> : null}
          {project.is_default ? (
            <span className="text-[11.5px] text-muted">
              The key of the default project is fixed — every insert path that does not pass a
              project relies on it.
            </span>
          ) : null}
        </div>
      </section>

      <section className="flex flex-col gap-2">
        <div className="flex items-center gap-2">
          <h2 className="text-[13.5px] font-medium">Members</h2>
          <span className="text-[11.5px] text-muted">
            {members.length} in the project · {ownerCount} owner{ownerCount === 1 ? "" : "s"}
          </span>
          <button
            type="button"
            data-project-add-member
            disabled={busy || archived}
            onClick={() => void addMember()}
            className="ml-auto inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] transition hover:text-ink disabled:opacity-60"
          >
            <UserPlus className="size-3.5" aria-hidden />
            Add member
          </button>
        </div>

        {rowError ? (
          <p role="alert" data-project-row-error className="text-[11.5px] text-red-600">
            {rowError}
          </p>
        ) : null}

        <div className="overflow-hidden rounded-xl border border-line bg-surface">
          <div className="overflow-x-auto">
            <table data-project-members className="w-full border-collapse text-left text-[13px]">
              <thead>
                <tr className="border-b border-line text-[11.5px] text-muted">
                  <th scope="col" className="px-3 py-2.5">Name</th>
                  <th scope="col" className="px-3 py-2.5">Email</th>
                  <th scope="col" className="px-3 py-2.5">Role</th>
                  <th scope="col" className="px-3 py-2.5">Can</th>
                  <th scope="col" className="px-3 py-2.5"><span className="sr-only">Actions</span></th>
                </tr>
              </thead>
              <tbody>
                {members.map((member) => {
                  const lastOwner = member.role === "owner" && ownerCount <= 1;
                  return (
                    <tr key={member.user_id} data-project-member={member.user_id} className="border-b border-line last:border-b-0">
                      <td className="px-3 py-2.5">{member.display_name}</td>
                      <td className="px-3 py-2.5 text-muted">{member.email}</td>
                      <td className="px-3 py-2.5">
                        <label className="sr-only" htmlFor={`role-${member.user_id}`}>
                          Role for {member.display_name}
                        </label>
                        <select
                          id={`role-${member.user_id}`}
                          data-project-role={member.user_id}
                          value={member.role}
                          disabled={busy || archived}
                          onChange={(event) => void changeRole(member, event.target.value as ProjectRole)}
                          className="rounded-lg border border-line bg-surface px-2 py-1 text-[12.5px] disabled:bg-quiet-soft"
                        >
                          {ROLES.map((role) => (
                            <option key={role} value={role}>
                              {role}
                            </option>
                          ))}
                        </select>
                      </td>
                      <td className="px-3 py-2.5 text-[11.5px] text-muted">{ROLE_ABILITY[member.role]}</td>
                      <td className="px-3 py-2.5 text-right">
                        <button
                          type="button"
                          data-project-remove-member={member.user_id}
                          disabled={busy || archived || lastOwner}
                          title={
                            lastOwner
                              ? "This is the last owner — transfer ownership before removing them"
                              : "Remove from this project"
                          }
                          onClick={() => void remove(member)}
                          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:text-ink disabled:opacity-50"
                        >
                          <Trash2 className="size-3.5" aria-hidden />
                          Remove
                        </button>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        </div>
        <p className="text-[11.5px] text-muted">
          At least one owner must remain: the last owner cannot be removed here, and the API
          refuses it too — a project with no owner is one nobody can administer.
        </p>
      </section>

      <section className="flex flex-col gap-2">
        <div className="flex items-center gap-2">
          <h2 className="text-[13.5px] font-medium">Workflows</h2>
          <span className="text-[11.5px] text-muted">
            {workflows.length} in this project
          </span>
        </div>

        {workflowsError ? (
          <p role="alert" data-project-workflows-error className="text-[11.5px] text-red-600">
            {workflowsError}
          </p>
        ) : null}

        {workflows.length === 0 && !workflowsError ? (
          <p data-project-workflows-empty className="text-[12px] text-muted">
            No workflows in this project. Anything created without a project lands in the
            organization&apos;s default one.
          </p>
        ) : null}

        {workflows.length > 0 ? (
          <div className="overflow-hidden rounded-xl border border-line bg-surface">
            <div className="overflow-x-auto">
              <table data-project-workflows className="w-full border-collapse text-left text-[13px]">
                <thead>
                  <tr className="border-b border-line text-[11.5px] text-muted">
                    <th scope="col" className="px-3 py-2.5">Name</th>
                    <th scope="col" className="px-3 py-2.5">Trigger</th>
                    <th scope="col" className="px-3 py-2.5"><span className="sr-only">Actions</span></th>
                  </tr>
                </thead>
                <tbody>
                  {workflows.map((workflow) => (
                    <tr key={workflow.id} data-project-workflow={workflow.id} className="border-b border-line last:border-b-0">
                      <td className="px-3 py-2.5">
                        {workflow.name}
                        {!workflow.enabled ? (
                          <span className="ml-2 tag tag-muted">Disabled</span>
                        ) : null}
                      </td>
                      <td className="px-3 py-2.5 text-[12px] text-muted">
                        {workflow.trigger}
                        {workflow.schedule ? ` · ${workflow.schedule}` : ""}
                      </td>
                      <td className="px-3 py-2.5 text-right">
                        <button
                          type="button"
                          data-project-move-workflow={workflow.id}
                          disabled={busy || archived}
                          title={
                            archived
                              ? "This project is archived, so it is read-only — restore it to move a workflow out"
                              : "Move to another project, with a dependency report first"
                          }
                          onClick={() => setMoving(workflow)}
                          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2 py-1 text-[11.5px] transition hover:text-ink disabled:opacity-50"
                        >
                          <FolderInput className="size-3.5" aria-hidden />
                          Move to project…
                        </button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>
        ) : null}
      </section>

      <section>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            data-project-transfer-ownership
            disabled={busy || archived || members.length < 2}
            title={
              members.length < 2
                ? "Nobody else is in this project yet — add a member before handing it over"
                : "Hand this project to another member, with two confirmations"
            }
            onClick={() => setHandingOver(true)}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] transition hover:text-ink disabled:opacity-50"
          >
            <ArrowRightLeft className="size-3.5" aria-hidden />
            Hand over ownership
          </button>
          {!project.is_default ? (
          <button
            type="button"
            data-project-toggle-archive
            disabled={busy}
            onClick={() => void toggleArchived()}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] transition hover:text-ink disabled:opacity-60"
          >
            {busy ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : archived ? (
              <ArchiveRestore className="size-3.5" aria-hidden />
            ) : (
              <Archive className="size-3.5" aria-hidden />
            )}
            {archived ? "Restore this project" : "Archive this project"}
          </button>
          ) : null}
        </div>
      </section>

      {handingOver ? (
        <TransferOwnershipDialog
          project={project}
          members={members}
          onClose={() => setHandingOver(false)}
          onTransferred={() => {
            setHandingOver(false);
            void load();
          }}
        />
      ) : null}

      {moving ? (
        <MoveDialog
          workflowId={moving.id}
          workflowName={moving.name}
          organizationId={project.organization_id}
          onClose={() => setMoving(null)}
          onMoved={() => {
            setMoving(null);
            void load();
          }}
        />
      ) : null}
    </div>
  );
}

"use client";

/**
 * `/settings/iam/simulator` — ask the decision path directly (REQ-006, slice 2).
 *
 * The verdict is not computed here: the API answers with the resolution the route guard runs, and
 * this screen renders it — the winning role (or the deny that took the permission away) and every
 * binding that was considered, including the rows that did not count because they are expired,
 * revoked or out of scope.
 */
import { useCallback, useEffect, useState } from "react";

import { Copy, Play, Scale } from "lucide-react";

import { useSession } from "@/lib/session";
import {
  ApiError,
  fetchIamGroups,
  fetchIamServiceAccounts,
  fetchIamUsers,
  fetchOrganizations,
  fetchPermissionCatalogue,
  fetchSites,
  runIamSimulation,
  type IamGroup,
  type IamPermissionDef,
  type IamServiceAccount,
  type IamSimulationReport,
  type IamUserRow,
} from "@/lib/api";
import type { Organization, Site } from "@/lib/types";

/** `/settings/iam/simulator`. */
export function SimulatorView() {
  const { user } = useSession();
  const [organizations, setOrganizations] = useState<Organization[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string | null>(null);
  const [users, setUsers] = useState<IamUserRow[]>([]);
  const [groups, setGroups] = useState<IamGroup[]>([]);
  const [accounts, setAccounts] = useState<IamServiceAccount[]>([]);
  const [sites, setSites] = useState<Site[]>([]);
  const [catalogue, setCatalogue] = useState<IamPermissionDef[]>([]);
  const [subjectType, setSubjectType] = useState<"user" | "group" | "service_account">("user");
  const [subjectId, setSubjectId] = useState("");
  const [permission, setPermission] = useState("");
  const [siteId, setSiteId] = useState("");
  const [path, setPath] = useState("");
  const [department, setDepartment] = useState("");
  const [moduleKey, setModuleKey] = useState("");
  const [report, setReport] = useState<IamSimulationReport | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const platformAccount = user ? user.organization_id === null : false;
  const activeOrg = platformAccount ? selectedOrg : (user?.organization_id ?? null);

  const loadOptions = useCallback(async (organizationId: string | null) => {
    try {
      const [userList, groupList, accountList, siteList] = await Promise.all([
        fetchIamUsers({ organizationId: organizationId ?? undefined }),
        fetchIamGroups(organizationId),
        fetchIamServiceAccounts(organizationId),
        organizationId ? fetchSites(organizationId) : Promise.resolve([] as Site[]),
      ]);
      setUsers(userList.users);
      setGroups(groupList);
      setAccounts(accountList);
      setSites(siteList);
    } catch {
      setUsers([]);
      setGroups([]);
      setAccounts([]);
      setSites([]);
    }
  }, []);

  useEffect(() => {
    if (!user) return;
    if (user.organization_id !== null) {
      void loadOptions(null);
      return;
    }
    if (organizations !== null) return;
    void fetchOrganizations()
      .then((list) => {
        setOrganizations(list);
        setSelectedOrg(list[0]?.id ?? null);
      })
      .catch(() => setOrganizations([]));
  }, [user, organizations, loadOptions]);

  useEffect(() => {
    if (platformAccount && selectedOrg) {
      void loadOptions(selectedOrg);
    }
  }, [platformAccount, selectedOrg, loadOptions]);

  useEffect(() => {
    void fetchPermissionCatalogue()
      .then(setCatalogue)
      .catch(() => setCatalogue([]));
  }, []);

  const run = async () => {
    setBusy(true);
    setError(null);
    setNotice(null);
    setReport(null);
    try {
      const answer = await runIamSimulation({
        subjectType,
        subjectId: subjectId || user?.id || "",
        permission,
        organizationId: activeOrg,
        siteId: siteId || null,
        path: path || undefined,
        department: department || undefined,
        module: moduleKey || undefined,
      });
      setReport(answer);
    } catch (cause) {
      setError(
        cause instanceof ApiError ? `${cause.message} (${cause.code})` : "The query could not run.",
      );
    } finally {
      setBusy(false);
    }
  };

  const subjects =
    subjectType === "user"
      ? users.map((entry) => ({ id: entry.id, label: entry.display_name || entry.email }))
      : subjectType === "group"
        ? groups.map((entry) => ({ id: entry.id, label: entry.name }))
        : accounts.map((entry) => ({ id: entry.id, label: entry.name }));

  const canRun = permission !== "" && (subjectId !== "" || subjectType === "user");

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        {platformAccount && organizations && organizations.length > 0 ? (
          <label className="flex items-center gap-2 text-[12.5px]">
            <span className="text-muted">Organization</span>
            <select
              value={selectedOrg ?? ""}
              data-sim-organization
              onChange={(event) => setSelectedOrg(event.target.value)}
              className="h-8 rounded-lg border border-line bg-surface px-2 text-[12.5px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            >
              {organizations.map((organization) => (
                <option key={organization.id} value={organization.id}>
                  {organization.name}
                </option>
              ))}
            </select>
          </label>
        ) : (
          <span />
        )}
        <p className="text-[12.5px] text-muted">
          The verdict equals the guard's verdict — both call the same resolution.
        </p>
      </div>

      <form
        className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
        data-sim-form
        onSubmit={(event) => {
          event.preventDefault();
          void run();
        }}
      >
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
          <label className="flex flex-col gap-1.5">
            <span className="text-[12.5px] font-medium text-ink">Subject</span>
            <select
              value={subjectType}
              data-sim-subject-type
              onChange={(event) => {
                setSubjectType(event.target.value as typeof subjectType);
                setSubjectId("");
              }}
              className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            >
              <option value="user">Account</option>
              <option value="group">Group</option>
              <option value="service_account">Service account</option>
            </select>
          </label>
          <label className="flex flex-col gap-1.5">
            <span className="text-[12.5px] font-medium text-ink">
              {subjectType === "user" ? "Account (defaults to you)" : "Which one"}
            </span>
            <select
              value={subjectId}
              data-sim-subject
              onChange={(event) => setSubjectId(event.target.value)}
              className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            >
              <option value="">
                {subjectType === "user" ? "Me" : `Pick a ${subjectType === "group" ? "group" : "identity"}…`}
              </option>
              {subjects.map((subject) => (
                <option key={subject.id} value={subject.id}>
                  {subject.label}
                </option>
              ))}
            </select>
          </label>
          <label className="flex flex-col gap-1.5">
            <span className="text-[12.5px] font-medium text-ink">Permission</span>
            <select
              value={permission}
              data-sim-permission
              onChange={(event) => setPermission(event.target.value)}
              className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            >
              <option value="">Pick a permission…</option>
              {catalogue.map((entry) => (
                <option key={entry.key} value={entry.key}>
                  {entry.key}
                </option>
              ))}
            </select>
          </label>
          <label className="flex flex-col gap-1.5">
            <span className="text-[12.5px] font-medium text-ink">Site (optional)</span>
            <select
              value={siteId}
              data-sim-site
              onChange={(event) => setSiteId(event.target.value)}
              className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            >
              <option value="">None</option>
              {sites.map((site) => (
                <option key={site.id} value={site.id}>
                  {site.name} ({site.key})
                </option>
              ))}
            </select>
          </label>
          <label className="flex flex-col gap-1.5">
            <span className="text-[12.5px] font-medium text-ink">Path (optional)</span>
            <input
              value={path}
              data-sim-path
              onChange={(event) => setPath(event.target.value)}
              placeholder="/blog/hello-world"
              className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
          </label>
          <div className="grid grid-cols-2 gap-3">
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Department</span>
              <input
                value={department}
                data-sim-department
                onChange={(event) => setDepartment(event.target.value)}
                placeholder="marketing"
                className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <label className="flex flex-col gap-1.5">
              <span className="text-[12.5px] font-medium text-ink">Module</span>
              <input
                value={moduleKey}
                data-sim-module
                onChange={(event) => setModuleKey(event.target.value)}
                placeholder="content"
                className="h-9 rounded-lg border border-line bg-surface px-2 text-[13px] text-ink outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
          </div>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="submit"
            disabled={busy || !canRun}
            data-sim-run
            className="flex items-center gap-1.5 rounded-lg bg-accent px-3.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:bg-accent-soft disabled:text-accent-strong"
          >
            <Play className="size-3.5" aria-hidden />
            Run
          </button>
          <span className="text-[11.5px] text-muted">
            A finer scope (department, module, path) only counts when the query names it.
          </span>
        </div>
      </form>

      {error ? (
        <p role="alert" data-sim-error className="rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-[12.5px] text-caution">
          {error}
        </p>
      ) : null}

      {report ? (
        <section className="flex flex-col gap-3" aria-label="Simulation verdict" data-sim-report>
          <div
            className={`flex flex-wrap items-center justify-between gap-3 rounded-xl border p-4 ${
              report.allowed
                ? "border-positive/40 bg-positive-soft"
                : "border-danger/50 bg-danger-soft"
            }`}
          >
            <div className="flex items-center gap-3">
              <Scale
                className={`size-5 ${report.allowed ? "text-positive" : "text-caution"}`}
                aria-hidden
              />
              <div>
                <p
                  className={`text-[15px] font-semibold ${report.allowed ? "text-positive" : "text-caution"}`}
                  data-sim-verdict
                >
                  {report.allowed ? "ALLOWED" : "DENIED"}
                </p>
                <p className="text-[12px] text-muted">
                  {report.subject} · {report.permission}
                  {report.context.path ? ` · ${report.context.path}` : ""}
                  {report.reason === "explicit_deny" ? " · an explicit deny won" : ""}
                  {report.reason === "missing_permission" ? " · nothing grants it" : ""}
                </p>
              </div>
            </div>
            <div className="flex items-center gap-2">
              <button
                type="button"
                data-sim-copy
                onClick={() => {
                  void navigator.clipboard?.writeText(
                    JSON.stringify(
                      {
                        subject_type: subjectType,
                        subject_id: report.subject,
                        permission: report.permission,
                        context: report.context,
                      },
                      null,
                      2,
                    ),
                  );
                  setNotice("The case was copied as JSON.");
                }}
                className="flex items-center gap-1.5 rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-quiet-soft"
              >
                <Copy className="size-3" aria-hidden />
                Copy as test case
              </button>
            </div>
          </div>

          {report.source ? (
            <p className="rounded-xl border border-line bg-surface px-3 py-2 text-[12.5px]" data-sim-source>
              Decided by <strong>{report.source.role_name}</strong>{" "}
              <span className="font-mono text-[11.5px] text-muted">({report.source.role_key})</span> —{" "}
              {report.source.via.replace("_", " ")}
              {report.source.via.includes("inherited") ? " through inheritance" : ""}.
            </p>
          ) : (
            <p className="rounded-xl border border-line bg-surface px-3 py-2 text-[12.5px]" data-sim-source>
              No role grants this permission, and no deny had to remove it.
            </p>
          )}

          <div className="overflow-x-auto rounded-xl border border-line bg-surface">
            <table className="w-full text-left">
              <thead className="bg-canvas text-[11.5px] tracking-wide text-muted uppercase">
                <tr>
                  <th className="px-3 py-2 font-medium">Binding</th>
                  <th className="px-3 py-2 font-medium">Role</th>
                  <th className="px-3 py-2 font-medium">Scope</th>
                  <th className="px-3 py-2 font-medium">State</th>
                  <th className="px-3 py-2 font-medium">Says</th>
                </tr>
              </thead>
              <tbody>
                {report.chain.map((step) => (
                  <tr
                    key={step.binding_id}
                    data-sim-step={step.binding_id}
                    data-sim-step-state={step.state}
                    className={`border-t border-line ${step.counts ? "" : "opacity-60"}`}
                  >
                    <td className="px-3 py-2 font-mono text-[11.5px] text-muted">
                      {step.binding_id.slice(0, 8)}…
                    </td>
                    <td className="px-3 py-2 text-[12.5px]">
                      {step.role_name}
                      <span className="ml-1.5 font-mono text-[11px] text-muted">{step.role_key}</span>
                    </td>
                    <td className="px-3 py-2 font-mono text-[11.5px] text-muted">{step.scope}</td>
                    <td className="px-3 py-2">
                      <span
                        className={`rounded-md border px-1.5 py-0.5 text-[11px] font-medium ${
                          step.counts
                            ? "border-positive/40 bg-positive-soft text-positive"
                            : "border-line bg-canvas text-muted"
                        }`}
                      >
                        {step.state.replace("_", " ")}
                      </span>
                    </td>
                    <td className="px-3 py-2 text-[12px]">
                      {step.effect ? (
                        <span className={step.effect === "deny" ? "text-caution" : "text-positive"}>
                          {step.effect}
                          {step.via ? ` · ${step.via.replace("_", " ")}` : ""}
                          {step.inherited_from ? ` (from ${step.inherited_from})` : ""}
                        </span>
                      ) : (
                        <span className="text-muted">—</span>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <p className="text-[11.5px] text-muted" data-sim-note>
            {report.note}
          </p>
        </section>
      ) : null}

      {notice ? (
        <p role="status" data-sim-notice className="rounded-lg border border-line bg-quiet-soft px-3 py-2 text-[12.5px]">
          {notice}
        </p>
      ) : null}
    </div>
  );
}

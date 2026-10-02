"use client";

/**
 * Installed node packages (`/modules/installed`): the installer ledger
 * (docs/requests/REQ-087, slice 4).
 *
 * This is the REQ's `/modules/installed` screen — the packages a tenant installed, what they
 * asked for, and what happens to the workflows that use them. Four claims the screen makes,
 * each one a place an installer screen misleads:
 *
 * 1. **The permissions a package asked for are shown, not summarised.** A package that asked
 *    for `credentials` can offer a credential type; one that did not cannot. "5 permissions"
 *    is a number nobody can reason about, and the permission list is the only thing on the row
 *    that tells a tenant what a package can do.
 * 2. **A removal that breaks something says what, before the button is pressed.** The REQ's
 *    sentence is "removal disables them and flags dependent workflows instead of breaking
 *    them", and "instead of breaking" only means something if the reader is *told* it is
 *    about to. The confirm step therefore renders the affected workflows by name, and the
 *    server's own `message` is rendered verbatim beside it rather than re-worded — a warning
 *    the client paraphrases is a warning the client can get wrong.
 * 3. **A disabled package is not a removed one, and the row says which.** Disabling keeps the
 *    ledger row and every workflow loadable; removing marks the row removed. Both are in the
 *    same table, so the state chip is the only thing distinguishing them, and a screen that
 *    painted them the same would make a reversible action look irreversible.
 * 4. **Installing pastes a manifest, and a refusal is rendered finding by finding.** The
 *    validator is the gate, and a refusal carries *every* finding — so the panel lists them
 *    all rather than the first. An author who has to install, read one error, fix it, install
 *    again has a validator, not a gate.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Link from "next/link";
import {
  Boxes,
  CircleAlert,
  ClipboardPaste,
  Package,
  RefreshCw,
  ShieldCheck,
  Trash2,
  TriangleAlert,
} from "lucide-react";

import {
  fetchNodePackages,
  fetchOrganizations,
  installNodePackage,
  removeNodePackage,
  setNodePackageEnabled,
  type ApiError,
} from "@/lib/api";
import type { NodePackage, Organization, PackageFinding } from "@/lib/types";
import { useSession } from "@/lib/session";

/** What the screen is doing, so the header never lies about it. */
type Phase = "loading" | "ready" | "error";

/** The one notice line: a success, a refusal, or a failure. */
type Notice = { tone: "ok" | "warn" | "error"; text: string } | null;

/** A row's per-key busy state, so one row's spinner cannot claim the whole table. */
type BusyKey = string | null;

/**
 * A permission's plain-language label.
 *
 * The permission names are the platform's vocabulary; this screen is the tenant's. A row that
 * says `credentials` is a word from the code, and a row that says "can offer a credential
 * type" is a sentence somebody can decide on.
 */
const PERMISSION_LABEL: Record<string, string> = {
  network: "makes outbound requests",
  workflows: "reads and writes workflows",
  credentials: "can offer a credential type",
  triggers: "can register a trigger",
  sandbox: "runs its code out of process",
};

export function InstalledPackages() {
  const { user } = useSession();
  // A tenant reads its own ledger. A platform account has no organization of its own, so it
  // names one — the API refuses `organization_required` otherwise, and the screen would
  // render that refusal where the ledger should be.
  const platformAccount = user ? user.organization_id === null : false;
  const [organizations, setOrganizations] = useState<Organization[] | null>(null);
  const [selectedOrg, setSelectedOrg] = useState<string | null>(null);
  const organizationId = platformAccount ? selectedOrg : (user?.organization_id ?? null);
  const [packages, setPackages] = useState<NodePackage[] | null>(null);
  const [phase, setPhase] = useState<Phase>("loading");
  const [notice, setNotice] = useState<Notice>(null);
  const [busy, setBusy] = useState<BusyKey>(null);
  const [findings, setFindings] = useState<PackageFinding[] | null>(null);
  const [removing, setRemoving] = useState<NodePackage | null>(null);
  const [installing, setInstalling] = useState(false);
  const [manifestText, setManifestText] = useState("");
  const errorRef = useRef<HTMLDivElement>(null);

  // A platform account's list depends on which organization is chosen, so there is nothing to
  // read until one is — saying so is better than rendering an error the reader caused by
  // simply arriving.
  const needsOrganization = platformAccount && selectedOrg === null;

  const load = useCallback(async () => {
    if (needsOrganization) {
      setPackages([]);
      setPhase("ready");
      return;
    }
    setPhase("loading");
    try {
      const page = await fetchNodePackages(organizationId);
      setPackages(page.packages);
      setPhase("ready");
    } catch (error) {
      setNotice({ tone: "error", text: describe(error) });
      setPhase("error");
    }
  }, [organizationId, needsOrganization]);

  useEffect(() => {
    void load();
  }, [load]);

  // A platform account picks which organization's ledger it is reading; a tenant never sees
  // the control, because it has exactly one ledger.
  useEffect(() => {
    if (!platformAccount || organizations !== null) return;
    void fetchOrganizations()
      .then((list) => {
        setOrganizations(list);
        setSelectedOrg(list[0]?.id ?? null);
      })
      .catch(() => setOrganizations([]));
  }, [platformAccount, organizations]);

  const total = packages?.length ?? 0;
  const enabled = useMemo(
    () => packages?.filter((entry) => entry.enabled).length ?? 0,
    [packages],
  );
  const permissions = useMemo(() => {
    const all = new Set<string>();
    for (const entry of packages ?? []) {
      for (const permission of entry.permissions) all.add(permission);
    }
    return [...all].sort();
  }, [packages]);

  /** Enable or disable, then re-read so the row shows what the server decided. */
  const toggle = useCallback(
    async (entry: NodePackage) => {
      setBusy(entry.key);
      setNotice(null);
      try {
        const result = await setNodePackageEnabled(entry.key, !entry.enabled, organizationId);
        setNotice({ tone: result.package.enabled ? "ok" : "warn", text: result.message });
        await load();
      } catch (error) {
        setNotice({ tone: "error", text: describe(error) });
      } finally {
        setBusy(null);
      }
    },
    [load],
  );

  /** Remove, showing the server's own report of what it touched. */
  const confirmRemove = useCallback(async () => {
    const entry = removing;
    if (!entry) return;
    setBusy(entry.key);
    setNotice(null);
    try {
      const result = await removeNodePackage(entry.key, organizationId);
      setNotice({
        tone: result.affected_workflows.length > 0 ? "warn" : "ok",
        text: result.message,
      });
      setRemoving(null);
      await load();
    } catch (error) {
      setNotice({ tone: "error", text: describe(error) });
    } finally {
      setBusy(null);
    }
  }, [load, removing]);

  /** Install from pasted JSON. Every finding of a refusal is rendered, not just the first. */
  const install = useCallback(async () => {
    setInstalling(true);
    setNotice(null);
    setFindings(null);
    let manifest: unknown;
    try {
      manifest = JSON.parse(manifestText);
    } catch (error) {
      setFindings([
        {
          code: "manifest_not_json",
          subject: "manifest.json",
          message: `that is not valid JSON: ${(error as Error).message}`,
        },
      ]);
      setInstalling(false);
      return;
    }
    try {
      const result = await installNodePackage(manifest, organizationId);
      const dropped = result.replaced_node_keys;
      setNotice({
        tone: "ok",
        text:
          `Installed ${result.key} ${result.version} — ${result.node_keys.length} node(s) available` +
          (dropped.length > 0 ? `. No longer shipped: ${dropped.join(", ")}` : ""),
      });
      setManifestText("");
      setInstalling(false);
      await load();
    } catch (error) {
      const api = error as ApiError;
      const details = api?.details;
      setFindings(
        Array.isArray(details)
          ? (details as PackageFinding[])
          : [
              {
                code: api?.code ?? "install_failed",
                subject: result0(manifest),
                message: api?.message ?? "the install failed",
              },
            ],
      );
      setInstalling(false);
      window.setTimeout(() => errorRef.current?.focus(), 0);
    }
  }, [load, manifestText, organizationId]);

  return (
    <div className="flex flex-col gap-4">
      {/* Header: the counts are the server's, and one line says what the screen is. */}
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <p className="text-[13px] text-muted">
          {phase === "loading"
            ? "Reading the installer ledger…"
            : `${total} package${total === 1 ? "" : "s"} installed · ${enabled} available · ` +
              `${permissions.length} distinct permission${permissions.length === 1 ? "" : "s"} requested`}
        </p>
        <div className="flex items-center gap-2">
          {platformAccount && organizations && organizations.length > 0 ? (
            <label className="flex items-center gap-2 text-[12.5px]">
              <span className="text-muted">Organization</span>
              <select
                value={selectedOrg ?? ""}
                data-testid="packages-organization"
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
          ) : null}
          <button
            type="button"
            onClick={() => void load()}
            data-testid="packages-refresh"
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[13px] hover:bg-quiet-soft"
          >
            <RefreshCw className="h-3.5 w-3.5" aria-hidden />
            Refresh
          </button>
          <Link
            href="/workflows/nodes"
            data-testid="packages-open-library"
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[13px] hover:bg-quiet-soft"
          >
            <Boxes className="h-3.5 w-3.5" aria-hidden />
            Node library
          </Link>
        </div>
      </div>

      {notice && (
        <p
          role="status"
          data-testid="packages-notice"
          className={
            notice.tone === "error"
              ? "rounded-md border border-red-500/30 bg-red-500/5 px-3 py-2 text-[13px] text-red-700 dark:text-red-300"
              : notice.tone === "warn"
                ? "rounded-md border border-amber-500/30 bg-amber-500/5 px-3 py-2 text-[13px] text-amber-700 dark:text-amber-300"
                : "rounded-md border border-emerald-500/30 bg-emerald-500/5 px-3 py-2 text-[13px] text-emerald-700 dark:text-emerald-300"
          }
        >
          {notice.text}
        </p>
      )}

      {/* The list. A skeleton keeps the table's shape so the page does not jump. */}
      {phase === "loading" && <PackageSkeleton />}

      {phase === "error" && packages === null && (
        <p className="rounded-md border border-line px-3 py-6 text-center text-[13px] text-muted">
          The installer ledger could not be read. The reason is above.
        </p>
      )}

      {phase === "ready" && packages !== null && packages.length === 0 && (
        <div
          data-testid="packages-empty"
          className="rounded-lg border border-dashed border-line px-4 py-10 text-center"
        >
          <Package className="mx-auto mb-2 h-6 w-6 text-muted" aria-hidden />
          <p className="text-[14px] font-medium">No node packages installed</p>
          <p className="mx-auto mt-1 max-w-md text-[13px] text-muted">
            Every node the canvas can place right now ships with Omnion. Install a package below
            — or write one with <code className="font-mono text-[12px]">omnion node scaffold</code>{" "}
            and validate it here first.
          </p>
        </div>
      )}

      {phase === "ready" && packages !== null && packages.length > 0 && (
        <div className="overflow-x-auto">
          <table className="w-full min-w-[46rem] text-left text-[13px]">
            <thead className="text-[12px] text-muted">
              <tr>
                <th scope="col" className="py-2 pr-3 font-medium">
                  Package
                </th>
                <th scope="col" className="py-2 pr-3 font-medium">
                  Nodes
                </th>
                <th scope="col" className="py-2 pr-3 font-medium">
                  Permissions
                </th>
                <th scope="col" className="py-2 pr-3 font-medium">
                  State
                </th>
                <th scope="col" className="py-2 font-medium text-right">
                  Actions
                </th>
              </tr>
            </thead>
            <tbody>
              {packages.map((entry) => (
                <tr
                  key={entry.key}
                  data-testid="package-row"
                  data-package-key={entry.key}
                  className="border-t border-line align-top"
                >
                  <td className="py-2.5 pr-3">
                    <div className="font-medium">{entry.key}</div>
                    <div className="text-[12px] text-muted">
                      {entry.version} · {entry.source}
                    </div>
                    <div
                      className="mt-1 font-mono text-[11px] text-muted"
                      title="The checksum the server computed over the canonical manifest"
                    >
                      {entry.checksum.slice(0, 12)}
                    </div>
                  </td>
                  <td className="py-2.5 pr-3">
                    {entry.node_keys.length === 0 ? (
                      <span className="text-[12px] text-muted">none recorded</span>
                    ) : (
                      <ul className="space-y-0.5">
                        {entry.node_keys.map((key) => (
                          <li key={key} className="font-mono text-[12px]">
                            {key}
                          </li>
                        ))}
                      </ul>
                    )}
                  </td>
                  <td className="py-2.5 pr-3">
                    <ul className="space-y-0.5">
                      {entry.permissions.map((permission) => (
                        <li key={permission} className="text-[12px]">
                          <span className="font-mono text-muted">{permission}</span>
                          <span className="ml-1.5 text-muted">
                            {PERMISSION_LABEL[permission] ?? "unknown permission"}
                          </span>
                        </li>
                      ))}
                    </ul>
                  </td>
                  <td className="py-2.5 pr-3">
                    <span
                      data-testid="package-state"
                      className={
                        entry.enabled
                          ? "inline-flex items-center gap-1 rounded-full bg-emerald-500/10 px-2 py-0.5 text-[12px] text-emerald-700 dark:text-emerald-300"
                          : "inline-flex items-center gap-1 rounded-full bg-amber-500/10 px-2 py-0.5 text-[12px] text-amber-700 dark:text-amber-300"
                      }
                    >
                      {entry.enabled ? (
                        <>
                          <ShieldCheck className="h-3 w-3" aria-hidden />
                          available
                        </>
                      ) : (
                        <>
                          <TriangleAlert className="h-3 w-3" aria-hidden />
                          disabled · workflows still load
                        </>
                      )}
                    </span>
                  </td>
                  <td className="py-2.5 text-right">
                    <div className="flex justify-end gap-1.5">
                      <button
                        type="button"
                        onClick={() => void toggle(entry)}
                        disabled={busy === entry.key}
                        data-testid="package-toggle"
                        data-enabled={entry.enabled ? "true" : "false"}
                        className="rounded-md border border-line px-2.5 py-1.5 text-[12px] hover:bg-quiet-soft disabled:opacity-50"
                      >
                        {entry.enabled ? "Disable" : "Enable"}
                      </button>
                      <button
                        type="button"
                        onClick={() => setRemoving(entry)}
                        disabled={busy === entry.key}
                        data-testid="package-remove"
                        className="inline-flex items-center gap-1 rounded-md border border-line px-2.5 py-1.5 text-[12px] text-red-700 hover:bg-red-500/5 disabled:opacity-50 dark:text-red-300"
                      >
                        <Trash2 className="h-3.5 w-3.5" aria-hidden />
                        Remove
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {/* The installer. A refusal renders every finding, because the validator reports them
          all and showing one would be a slower version of the same information. */}
      <section className="rounded-lg border border-line p-4" aria-labelledby="install-heading">
        <h2 id="install-heading" className="flex items-center gap-2 text-[14px] font-medium">
          <ClipboardPaste className="h-4 w-4" aria-hidden />
          Install a package
        </h2>
        <p className="mt-1 text-[13px] text-muted">
          Paste a <code className="font-mono text-[12px]">manifest.json</code> or a packed{" "}
          <code className="font-mono text-[12px]">*.omnion-node.json</code>. The server validates
          it before anything is recorded, and a package that fails installs nothing.
        </p>
        {/* The install button is disabled until an organization is chosen, so the reason is
            stated here rather than leaving a control that cannot be pressed. */}
        {needsOrganization ? (
          <p
            className="mt-2 flex items-center gap-2 text-[13px] text-muted"
            data-testid="packages-needs-organization"
          >
            <TriangleAlert className="h-3.5 w-3.5" aria-hidden />
            Choose an organization above to read its ledger and install into it.
          </p>
        ) : null}
        <label htmlFor="package-manifest" className="mt-3 block text-[13px] font-medium">
          Manifest JSON
        </label>
        <textarea
          id="package-manifest"
          data-testid="package-manifest"
          value={manifestText}
          onChange={(event) => setManifestText(event.target.value)}
          rows={6}
          spellCheck={false}
          placeholder='{ "key": "acme-tools", "version": "0.1.0", … }'
          className="mt-1 w-full rounded-md border border-line bg-quiet-soft px-3 py-2 font-mono text-[12px]"
        />
        <div className="mt-2 flex items-center gap-2">
          <button
            type="button"
            onClick={() => void install()}
            disabled={installing || needsOrganization || manifestText.trim() === ""}
            data-testid="package-install"
            className="rounded-md bg-ink px-3 py-1.5 text-[13px] text-background disabled:opacity-50"
          >
            {installing ? "Validating…" : "Validate and install"}
          </button>
          {manifestText.trim() !== "" && (
            <button
              type="button"
              onClick={() => {
                setManifestText("");
                setFindings(null);
              }}
              data-testid="package-install-clear"
              className="rounded-md border border-line px-3 py-1.5 text-[13px] hover:bg-quiet-soft"
            >
              Clear
            </button>
          )}
        </div>

        {findings && findings.length > 0 && (
          <div
            ref={errorRef}
            tabIndex={-1}
            role="alert"
            data-testid="package-findings"
            className="mt-3 rounded-md border border-red-500/30 bg-red-500/5 p-3 outline-none"
          >
            <p className="flex items-center gap-1.5 text-[13px] font-medium text-red-700 dark:text-red-300">
              <CircleAlert className="h-3.5 w-3.5" aria-hidden />
              The package was refused — {findings.length} finding{findings.length === 1 ? "" : "s"},
              and nothing was installed
            </p>
            <ul className="mt-2 space-y-1.5">
              {findings.map((finding, index) => (
                <li key={`${finding.code}-${index}`} className="text-[12px]">
                  <span className="font-mono text-red-700 dark:text-red-300">{finding.code}</span>
                  {finding.subject && <span className="text-muted"> · {finding.subject}</span>}
                  <span className="block text-muted">{finding.message}</span>
                </li>
              ))}
            </ul>
          </div>
        )}
      </section>

      {/* The removal confirmation. It names the workflows *before* the button, because the
          REQ's promise is that a removal flags rather than breaks — and a flag shown after
          the fact is a post-mortem. */}
      {removing && (
        <div
          role="dialog"
          aria-modal="true"
          aria-labelledby="remove-heading"
          data-testid="package-remove-dialog"
          className="rounded-lg border border-amber-500/40 bg-amber-500/5 p-4"
        >
          <h2 id="remove-heading" className="text-[14px] font-medium">
            Remove {removing.key}?
          </h2>
          <p className="mt-1 text-[13px]">
            Its {removing.node_keys.length} node(s) stop being offered in the palette. Every
            workflow that already uses one keeps loading — nothing is deleted or edited — and
            those nodes show as unavailable on the canvas with this package named as the cause.
          </p>
          <p className="mt-2 font-mono text-[11px] text-muted">
            {removing.node_keys.join(", ") || "no node keys recorded"}
          </p>
          <div className="mt-3 flex gap-2">
            <button
              type="button"
              onClick={() => void confirmRemove()}
              disabled={busy === removing.key}
              data-testid="package-remove-confirm"
              className="rounded-md border border-red-500/40 px-3 py-1.5 text-[13px] text-red-700 hover:bg-red-500/10 disabled:opacity-50 dark:text-red-300"
            >
              Remove the package
            </button>
            <button
              type="button"
              onClick={() => setRemoving(null)}
              data-testid="package-remove-cancel"
              className="rounded-md border border-line px-3 py-1.5 text-[13px] hover:bg-quiet-soft"
            >
              Keep it
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

/** The key of a pasted manifest, for a refusal that arrived without one. */
function result0(manifest: unknown): string {
  if (manifest && typeof manifest === "object" && "key" in manifest) {
    return String((manifest as { key: unknown }).key);
  }
  return "manifest.json";
}

/** A human sentence for anything thrown by the client. */
function describe(error: unknown): string {
  const api = error as ApiError;
  if (api?.message) return `${api.code ? `${api.code}: ` : ""}${api.message}`;
  if (error instanceof Error) return error.message;
  return "Something went wrong.";
}

/** Keeps the table's height while the ledger loads, so the page does not jump. */
function PackageSkeleton() {
  return (
    <div data-testid="packages-skeleton" className="space-y-2" aria-hidden>
      {[0, 1, 2].map((row) => (
        <div key={row} className="h-11 animate-pulse rounded-md bg-quiet-soft" />
      ))}
    </div>
  );
}

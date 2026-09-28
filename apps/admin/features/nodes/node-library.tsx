"use client";

/**
 * The node library (`/workflows/nodes`): everything the palette can place, read from the
 * registry.
 *
 * Four claims this screen makes, each one a way a library lies to the person reading it:
 *
 * 1. **Nothing here is hard-coded.** Every node, port, parameter and credential type arrives
 *    from `GET /api/v1/node-types`. A node that exists only in this file would be a node the
 *    canvas cannot place and the engine cannot run — and the divergence would only show up
 *    when somebody tried to use it.
 * 2. **The counts are the server's.** "12 of 40" reads `matched` and `total` off the response
 *    rather than counting the array on screen, so a filter the server applied and the client
 *    silently ignored cannot produce two different numbers in two different places.
 * 3. **A deprecated node is shown, not hidden.** It carries the replacement in the same chip,
 *    because a workflow may already depend on it and a library that pretends otherwise is
 *    asking the reader to trust it.
 * 4. **An empty result says what was searched.** "No node matches `postgrs`" and "the library
 *    is empty" are different problems with the same pixels, and only one of them is the
 *    reader's typing.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useRouter, useSearchParams } from "next/navigation";
import {
  ArrowRight,
  Box,
  CircleSlash,
  ExternalLink,
  KeyRound,
  Search,
  ShieldCheck,
  TriangleAlert,
  X,
} from "lucide-react";

import {
  fetchCredentialTypes,
  fetchNodeCategories,
  fetchNodeTypes,
  type ApiError,
} from "@/lib/api";
import type {
  CredentialType,
  NodeCategory,
  NodeType,
  NodeTypeFilters,
  NodeTypePage,
} from "@/lib/types";

/** Colour per library state. A deprecated node is amber, not red: it still runs. */
const STATE_TONE: Record<string, string> = {
  available: "bg-quiet-soft text-muted",
  deprecated: "bg-amber-500/10 text-amber-700 dark:text-amber-300",
  node_package_missing: "bg-red-500/10 text-red-700 dark:text-red-300",
};

const STATE_LABEL: Record<string, string> = {
  available: "Available",
  deprecated: "Deprecated",
  node_package_missing: "Package missing",
};

/** Read the filters out of the query string, so a filtered library can be pasted. */
function filtersFrom(params: URLSearchParams): NodeTypeFilters {
  const filters: NodeTypeFilters = {};
  const search = params.get("search");
  if (search) filters.search = search;
  const category = params.get("category");
  if (category) filters.category = category;
  const capability = params.get("capability");
  if (capability) filters.capability = capability;
  if (params.get("deprecated") === "false") filters.include_deprecated = false;
  if (params.get("credential") === "true") filters.credential = true;
  if (params.get("credential") === "false") filters.credential = false;
  return filters;
}

export function NodeLibrary() {
  const router = useRouter();
  const params = useSearchParams();
  const filters = useMemo(() => filtersFrom(params), [params]);

  const [page, setPage] = useState<NodeTypePage | null>(null);
  const [categories, setCategories] = useState<NodeCategory[]>([]);
  const [credentialTypes, setCredentialTypes] = useState<CredentialType[]>([]);
  const [error, setError] = useState<ApiError | null>(null);
  const [loading, setLoading] = useState(true);
  const [openKey, setOpenKey] = useState<string | null>(null);

  // `/` focuses the search box and `Esc` clears it — the same two bindings the notification
  // list uses, so the panel has one set of habits rather than one per screen.
  const searchRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "/" && !event.metaKey && !event.ctrlKey) {
        const target = event.target as HTMLElement | null;
        const typing =
          target?.tagName === "INPUT" ||
          target?.tagName === "TEXTAREA" ||
          target?.isContentEditable;
        if (!typing) {
          event.preventDefault();
          searchRef.current?.focus();
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // The category tree and the credential catalogue are static for the life of a release, so
  // they load once. The node list reloads whenever a filter changes.
  useEffect(() => {
    let alive = true;
    Promise.all([fetchNodeCategories(), fetchCredentialTypes()])
      .then(([categoryPage, credentialPage]) => {
        if (!alive) return;
        setCategories(categoryPage.categories);
        setCredentialTypes(credentialPage.types);
      })
      .catch((cause: ApiError) => {
        if (alive) setError(cause);
      });
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => {
    let alive = true;
    setLoading(true);
    fetchNodeTypes(filters)
      .then((result) => {
        if (!alive) return;
        setPage(result);
        setError(null);
      })
      .catch((cause: ApiError) => {
        if (!alive) return;
        setError(cause);
        setPage(null);
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [filters]);

  const setFilter = useCallback(
    (patch: Partial<NodeTypeFilters>) => {
      const next = new URLSearchParams();
      const merged = { ...filters, ...patch };
      if (merged.search) next.set("search", merged.search);
      if (merged.category) next.set("category", merged.category);
      if (merged.capability) next.set("capability", merged.capability);
      if (merged.include_deprecated === false) next.set("deprecated", "false");
      if (merged.credential !== undefined) next.set("credential", String(merged.credential));
      const query = next.toString();
      router.replace(query ? `/workflows/nodes?${query}` : "/workflows/nodes");
    },
    [filters, router],
  );

  const hasFilters =
    Boolean(filters.search) ||
    Boolean(filters.category) ||
    Boolean(filters.capability) ||
    filters.include_deprecated === false ||
    filters.credential !== undefined;

  const credentialLabel = (key: string) =>
    credentialTypes.find((type) => type.key === key)?.label ?? key;

  return (
    <div className="space-y-5">
      {/* Filters. The counts beside them are the server's, not a client recount. */}
      <div className="rounded-xl border border-line bg-quiet-soft/40 p-4">
        <div className="flex flex-wrap items-end gap-3">
          <label className="flex min-w-[220px] flex-1 flex-col gap-1">
            <span className="text-[12px] font-medium text-muted">Search</span>
            <span className="relative">
              <Search
                aria-hidden
                className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-muted"
                size={15}
              />
              <input
                ref={searchRef}
                type="search"
                value={filters.search ?? ""}
                placeholder="Label, key or what it does"
                onChange={(event) => setFilter({ search: event.target.value })}
                className="w-full rounded-lg border border-line bg-surface py-2 pl-9 pr-8 text-[13px] outline-none focus-visible:ring-2 focus-visible:ring-accent"
              />
              {filters.search ? (
                <button
                  type="button"
                  aria-label="Clear the search"
                  onClick={() => setFilter({ search: "" })}
                  className="absolute right-2 top-1/2 -translate-y-1/2 rounded p-1 text-muted hover:text-ink"
                >
                  <X size={14} />
                </button>
              ) : null}
            </span>
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-[12px] font-medium text-muted">Category</span>
            <select
              value={filters.category ?? ""}
              onChange={(event) => setFilter({ category: event.target.value })}
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none focus-visible:ring-2 focus-visible:ring-accent"
            >
              <option value="">All categories</option>
              {categories.map((category) => (
                <option key={category.key} value={category.key}>
                  {category.label} ({category.count})
                </option>
              ))}
            </select>
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-[12px] font-medium text-muted">Capability</span>
            <select
              value={filters.capability ?? ""}
              onChange={(event) => setFilter({ capability: event.target.value })}
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none focus-visible:ring-2 focus-visible:ring-accent"
            >
              <option value="">Any</option>
              <option value="execute">Runs in a workflow</option>
              <option value="trigger">Starts a run</option>
              <option value="webhook">Receives HTTP</option>
              <option value="poll">Polls on a schedule</option>
            </select>
          </label>

          <label className="flex flex-col gap-1">
            <span className="text-[12px] font-medium text-muted">Credential</span>
            <select
              value={
                filters.credential === undefined
                  ? ""
                  : String(filters.credential)
              }
              onChange={(event) =>
                setFilter({
                  credential:
                    event.target.value === "" ? undefined : event.target.value === "true",
                })
              }
              className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none focus-visible:ring-2 focus-visible:ring-accent"
            >
              <option value="">Any</option>
              <option value="true">Needs one</option>
              <option value="false">Needs none</option>
            </select>
          </label>

          <label className="flex items-center gap-2 pb-2 text-[13px]">
            <input
              type="checkbox"
              checked={filters.include_deprecated !== false}
              onChange={(event) => setFilter({ include_deprecated: event.target.checked })}
              className="h-4 w-4 rounded border-line"
            />
            Show deprecated
          </label>

          {hasFilters ? (
            <button
              type="button"
              onClick={() => router.replace("/workflows/nodes")}
              className="pb-2 text-[13px] text-accent underline underline-offset-2"
            >
              Clear filters
            </button>
          ) : null}
        </div>

        <p className="mt-3 text-[12px] text-muted" data-node-count>
          {loading && !page
            ? "Loading the registry…"
            : page
              ? `${page.matched} of ${page.total} nodes · ${page.filters.bundled_count} bundled with the release`
              : " "}
        </p>
      </div>

      {error ? (
        <p
          role="alert"
          className="flex items-center gap-2 rounded-xl border border-red-500/40 bg-red-500/10 px-4 py-3 text-[13px] text-red-700 dark:text-red-300"
        >
          <TriangleAlert size={15} />
          {error.message}
        </p>
      ) : null}

      {/* A search that matched nothing and a library that has nothing are different problems,
          and only one of them is the reader's typing. */}
      {!loading && page && page.nodes.length === 0 ? (
        <div className="rounded-xl border border-dashed border-line px-6 py-12 text-center">
          <CircleSlash aria-hidden className="mx-auto mb-3 text-muted" size={24} />
          <p className="text-[14px] font-medium">
            {hasFilters ? "No node matches these filters" : "The registry is empty"}
          </p>
          <p className="mx-auto mt-1 max-w-md text-[13px] text-muted">
            {hasFilters
              ? `Nothing in the registry matches ${describeFilters(filters)}. The search looks at a node's label, its key and what it does.`
              : "A release that ships without nodes would give the palette nothing to place."}
          </p>
          {hasFilters ? (
            <button
              type="button"
              onClick={() => router.replace("/workflows/nodes")}
              className="mt-4 inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-2 text-[13px] hover:bg-quiet-soft"
            >
              Clear filters
            </button>
          ) : null}
        </div>
      ) : null}

      {loading && !page ? (
        <ul className="space-y-2" aria-hidden>
          {[0, 1, 2].map((row) => (
            <li
              key={row}
              className="h-[74px] animate-pulse rounded-xl border border-line bg-quiet-soft/30"
            />
          ))}
        </ul>
      ) : null}

      <ul className="space-y-2">
        {(page?.nodes ?? []).map((node) => (
          <NodeRow
            key={node.key}
            node={node}
            open={openKey === node.key}
            onToggle={() => setOpenKey(openKey === node.key ? null : node.key)}
            credentialLabel={credentialLabel}
          />
        ))}
      </ul>
    </div>
  );
}

/** Put a filtered library back into words, so the empty state can name what was searched. */
function describeFilters(filters: NodeTypeFilters): string {
  const parts: string[] = [];
  if (filters.search) parts.push(`“${filters.search}”`);
  if (filters.category) parts.push(`the ${filters.category} category`);
  if (filters.capability) parts.push(`the ${filters.capability} capability`);
  if (filters.include_deprecated === false) parts.push("no deprecated nodes");
  if (filters.credential === true) parts.push("only nodes that need a credential");
  if (filters.credential === false) parts.push("only nodes that need none");
  return parts.join(", ") || "these filters";
}

function NodeRow({
  node,
  open,
  onToggle,
  credentialLabel,
}: {
  node: NodeType;
  open: boolean;
  onToggle: () => void;
  credentialLabel: (key: string) => string;
}) {
  return (
    <li className="rounded-xl border border-line bg-surface">
      <button
        type="button"
        onClick={onToggle}
        aria-expanded={open}
        data-node-row={node.key}
        className="flex w-full items-start gap-3 px-4 py-3 text-left"
      >
        <span
          aria-hidden
          className="mt-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-quiet-soft text-muted"
        >
          <Box size={16} />
        </span>
        <span className="min-w-0 flex-1">
          <span className="flex flex-wrap items-center gap-2">
            <span className="text-[14px] font-medium">{node.label}</span>
            <code className="rounded bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted">
              {node.key}
            </code>
            <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
              v{node.version}
            </span>
            <span
              className={`rounded-full px-2 py-0.5 text-[11px] ${
                STATE_TONE[node.state] ?? STATE_TONE.available
              }`}
              data-node-state={node.state}
            >
              {STATE_LABEL[node.state] ?? node.state}
            </span>
            {node.sandbox === "required" ? (
              <span
                title="This node's code runs out of process, never in the browser."
                className="inline-flex items-center gap-1 rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted"
              >
                <ShieldCheck size={11} />
                sandboxed
              </span>
            ) : null}
          </span>
          <span className="mt-1 block text-[13px] text-muted">{node.description}</span>
          {node.state_reason ? (
            <span className="mt-1 flex items-center gap-1.5 text-[12px] text-amber-700 dark:text-amber-300">
              <TriangleAlert size={12} />
              {node.state_reason}
            </span>
          ) : null}
        </span>
        <ArrowRight
          aria-hidden
          size={15}
          className={`mt-2 shrink-0 text-muted transition-transform ${open ? "rotate-90" : ""}`}
        />
      </button>

      {open ? <NodeDetail node={node} credentialLabel={credentialLabel} /> : null}
    </li>
  );
}

function NodeDetail({
  node,
  credentialLabel,
}: {
  node: NodeType;
  credentialLabel: (key: string) => string;
}) {
  return (
    <div className="border-t border-line px-4 py-4">
      <div className="grid gap-5 lg:grid-cols-2">
        <section>
          <h3 className="text-[12px] font-semibold uppercase tracking-wide text-muted">
            Ports
          </h3>
          <div className="mt-2 space-y-2">
            <PortList title="Inputs" ports={node.inputs} empty="This node takes no input." />
            <PortList title="Outputs" ports={node.outputs} />
          </div>
        </section>

        <section>
          <h3 className="text-[12px] font-semibold uppercase tracking-wide text-muted">
            Parameters
          </h3>
          {node.params.length === 0 ? (
            <p className="mt-2 text-[13px] text-muted">
              This node takes no parameters — it does one thing with what it is given.
            </p>
          ) : (
            <ul className="mt-2 space-y-2">
              {node.params.map((param) => (
                <li key={param.name} className="rounded-lg border border-line px-3 py-2">
                  <p className="flex flex-wrap items-center gap-2 text-[13px]">
                    <span className="font-medium">{param.label}</span>
                    <code className="rounded bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted">
                      {param.name}
                    </code>
                    <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
                      {param.ui}
                    </span>
                    {param.required ? (
                      <span className="rounded-full bg-amber-500/10 px-2 py-0.5 text-[11px] text-amber-700 dark:text-amber-300">
                        required
                      </span>
                    ) : null}
                  </p>
                  {param.help ? (
                    <p className="mt-1 text-[12px] text-muted">{param.help}</p>
                  ) : null}
                  {param.placeholder ? (
                    <p className="mt-1 text-[12px] text-muted">
                      e.g. <code className="text-muted/80">{param.placeholder}</code>
                    </p>
                  ) : null}
                  {param.secret_field ? (
                    <p className="mt-1 flex items-center gap-1.5 text-[12px] text-muted">
                      <KeyRound size={12} />
                      Takes a credential key, never a secret value.
                    </p>
                  ) : null}
                </li>
              ))}
            </ul>
          )}
        </section>
      </div>

      <dl className="mt-5 flex flex-wrap gap-x-6 gap-y-2 text-[12px] text-muted">
        <div>
          <dt className="inline">Category: </dt>
          <dd className="inline">{node.category.replace("_", " ")}</dd>
        </div>
        <div>
          <dt className="inline">Capabilities: </dt>
          <dd className="inline">{node.capabilities.join(", ")}</dd>
        </div>
        <div>
          <dt className="inline">Default attempts: </dt>
          <dd className="inline">{node.default_max_attempts}</dd>
        </div>
        {node.credential_types.length > 0 ? (
          <div>
            <dt className="inline">Credentials: </dt>
            <dd className="inline">
              {node.credential_types.map(credentialLabel).join(", ")}
            </dd>
          </div>
        ) : null}
      </dl>

      <div className="mt-4 flex flex-wrap gap-2">
        <a
          href={node.docs_url}
          target="_blank"
          rel="noreferrer"
          className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-2 text-[13px] hover:bg-quiet-soft"
        >
          <ExternalLink size={14} />
          Documentation
        </a>
        {node.superseded_by ? (
          <span className="inline-flex items-center gap-1.5 rounded-lg border border-amber-500/40 px-3 py-2 text-[13px] text-amber-700 dark:text-amber-300">
            Moved to {node.superseded_by}
          </span>
        ) : null}
      </div>
    </div>
  );
}

function PortList({
  title,
  ports,
  empty,
}: {
  title: string;
  ports: NodeType["inputs"];
  empty?: string;
}) {
  if (ports.length === 0) {
    return (
      <div>
        <p className="text-[12px] text-muted">{title}</p>
        <p className="mt-1 text-[13px] text-muted">{empty ?? "None."}</p>
      </div>
    );
  }
  return (
    <div>
      <p className="text-[12px] text-muted">{title}</p>
      <ul className="mt-1 flex flex-wrap gap-2">
        {ports.map((port) => (
          <li
            key={`${title}-${port.name}`}
            className="rounded-lg border border-line px-2.5 py-1.5 text-[12px]"
          >
            <span className="font-medium">{port.name}</span>
            <span className="ml-1.5 text-muted">{port.kind}</span>
            {port.accepts.length > 0 ? (
              <span className="ml-1.5 text-muted">({port.accepts.join(", ")})</span>
            ) : null}
            {!port.open ? <span className="ml-1.5 text-muted">closed</span> : null}
          </li>
        ))}
      </ul>
    </div>
  );
}

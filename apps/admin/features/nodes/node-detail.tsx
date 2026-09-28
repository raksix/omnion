"use client";

/**
 * One node in full (`/workflows/nodes/[key]`).
 *
 * A detail page exists so a link to a node survives: the palette points at it, a workflow's
 * "what was this?" links at it, and a person who bookmarked a node before a release renamed
 * it lands on the replacement rather than a dead route. Three claims it makes:
 *
 * 1. **The definition is the server's.** Ports, parameters, capabilities and the credential
 *    types it accepts all come from `GET /api/v1/node-types/{key}`; nothing here is transcribed
 *    from the registry into a second copy that can drift.
 * 2. **A deprecated node says so first, and names the replacement.** The banner is the first
 *    thing on the page, not a chip the reader has to notice, because the reader arrived here
 *    from a workflow that still uses it.
 * 3. **A key that resolves to nothing is a 404 with the key echoed**, not an empty detail
 *    page — an empty page reads as "this node has no parameters", which is a different and
 *    wrong claim.
 */
import { use, useEffect, useState } from "react";
import Link from "next/link";
import {
  ArrowLeft,
  Box,
  ExternalLink,
  KeyRound,
  ShieldCheck,
  TriangleAlert,
} from "lucide-react";

import { fetchNodeType, type ApiError } from "@/lib/api";
import type { NodePort, NodeType } from "@/lib/types";

export function NodeDetail({ nodeKey }: { nodeKey: string }) {
  const [node, setNode] = useState<NodeType | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    let alive = true;
    setLoading(true);
    fetchNodeType(nodeKey)
      .then((result) => {
        if (!alive) return;
        setNode(result);
        setError(null);
      })
      .catch((cause: ApiError) => {
        if (!alive) return;
        setError(cause);
        setNode(null);
      })
      .finally(() => {
        if (alive) setLoading(false);
      });
    return () => {
      alive = false;
    };
  }, [nodeKey]);

  if (loading) {
    return (
      <div className="space-y-3" aria-hidden>
        <div className="h-8 w-64 animate-pulse rounded-lg bg-quiet-soft/40" />
        <div className="h-40 animate-pulse rounded-xl border border-line bg-quiet-soft/30" />
      </div>
    );
  }

  if (error || !node) {
    return (
      <div className="rounded-xl border border-dashed border-line px-6 py-12 text-center">
        <TriangleAlert aria-hidden className="mx-auto mb-3 text-muted" size={24} />
        <p className="text-[14px] font-medium">This node is not in the registry</p>
        <p className="mx-auto mt-1 max-w-md text-[13px] text-muted">
          {error
            ? error.message
            : `No node is registered under “${nodeKey}”. It may have been removed in a release.`}
        </p>
        <Link
          href="/workflows/nodes"
          className="mt-4 inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-2 text-[13px] hover:bg-quiet-soft"
        >
          <ArrowLeft size={14} />
          Back to the library
        </Link>
      </div>
    );
  }

  return (
    <div className="space-y-5">
      <Link
        href="/workflows/nodes"
        className="inline-flex items-center gap-1.5 text-[13px] text-muted hover:text-ink"
      >
        <ArrowLeft size={14} />
        Node library
      </Link>

      {/* A deprecated node's banner is first, not a chip: the reader arrived from a workflow
          that still uses it, and the replacement is the thing they came for. */}
      {node.deprecated ? (
        <div
          role="status"
          className="flex flex-wrap items-center gap-2 rounded-xl border border-amber-500/40 bg-amber-500/10 px-4 py-3 text-[13px] text-amber-800 dark:text-amber-200"
        >
          <TriangleAlert size={15} />
          <span>This version is deprecated.</span>
          {node.superseded_by ? (
            <Link
              href={`/workflows/nodes/${node.superseded_by}`}
              className="inline-flex items-center gap-1 underline underline-offset-2"
            >
              Use {node.superseded_by} instead
              <ExternalLink size={12} />
            </Link>
          ) : (
            <span>There is no replacement registered yet.</span>
          )}
        </div>
      ) : null}

      <header className="flex flex-wrap items-start gap-3">
        <span
          aria-hidden
          className="mt-0.5 flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-quiet-soft text-muted"
        >
          <Box size={18} />
        </span>
        <div className="min-w-0">
          <h2 className="flex flex-wrap items-center gap-2 text-[18px] font-semibold">
            {node.label}
            <code className="rounded bg-quiet-soft px-1.5 py-0.5 text-[12px] text-muted">
              {node.key}
            </code>
            <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
              v{node.version}
            </span>
          </h2>
          <p className="mt-1 max-w-2xl text-[13px] text-muted">{node.description}</p>
          <div className="mt-2 flex flex-wrap gap-1.5">
            {node.capabilities.map((capability) => (
              <span
                key={capability}
                className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted"
              >
                {capability}
              </span>
            ))}
            {node.sandbox === "required" ? (
              <span className="inline-flex items-center gap-1 rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
                <ShieldCheck size={11} />
                out-of-process
              </span>
            ) : null}
          </div>
        </div>
      </header>

      <div className="grid gap-5 lg:grid-cols-2">
        <section className="rounded-xl border border-line p-4">
          <h3 className="text-[12px] font-semibold uppercase tracking-wide text-muted">
            Ports
          </h3>
          <PortTable title="Inputs" ports={node.inputs} />
          <PortTable title="Outputs" ports={node.outputs} />
        </section>

        <section className="rounded-xl border border-line p-4">
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
                <li key={param.name} className="rounded-lg bg-quiet-soft/40 px-3 py-2">
                  <p className="flex flex-wrap items-center gap-2 text-[13px]">
                    <span className="font-medium">{param.label}</span>
                    <code className="rounded bg-surface px-1.5 py-0.5 text-[11px] text-muted">
                      {param.name}
                    </code>
                    <span className="rounded-full bg-surface px-2 py-0.5 text-[11px] text-muted">
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
                  {param.options.length > 0 ? (
                    <p className="mt-1 text-[12px] text-muted">
                      Options: {param.options.join(", ")}
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

          {node.credential_types.length > 0 ? (
            <div className="mt-4 rounded-lg bg-quiet-soft/40 px-3 py-2">
              <p className="text-[12px] text-muted">Credential types it accepts</p>
              <p className="mt-1 flex flex-wrap gap-1.5">
                {node.credential_types.map((key) => (
                  <Link
                    key={key}
                    href={`/workflows/credentials/new?type=${encodeURIComponent(key)}`}
                    className="rounded-full bg-surface px-2 py-0.5 text-[11px] text-muted hover:text-ink"
                  >
                    {key}
                  </Link>
                ))}
              </p>
            </div>
          ) : null}
        </section>
      </div>

      <a
        href={node.docs_url}
        target="_blank"
        rel="noreferrer"
        className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-2 text-[13px] hover:bg-quiet-soft"
      >
        <ExternalLink size={14} />
        Read the documentation
      </a>
    </div>
  );
}

function PortTable({ title, ports }: { title: string; ports: NodePort[] }) {
  return (
    <div className="mt-3">
      <p className="text-[12px] text-muted">{title}</p>
      {ports.length === 0 ? (
        <p className="mt-1 text-[13px] text-muted">None.</p>
      ) : (
        <ul className="mt-1 space-y-1">
          {ports.map((port) => (
            <li
              key={`${title}-${port.name}`}
              className="flex flex-wrap items-center gap-2 rounded-lg border border-line px-2.5 py-1.5 text-[12px]"
            >
              <span className="font-medium">{port.name}</span>
              <span className="rounded-full bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted">
                {port.kind}
              </span>
              {port.accepts.length > 0 ? (
                <span className="text-muted">accepts {port.accepts.join(", ")}</span>
              ) : (
                <span className="text-muted">accepts anything</span>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** The page wrapper. `use` unwraps the route param so the fetch can start during the render. */
export function NodeDetailPage({ params }: { params: Promise<{ key: string }> }) {
  const { key } = use(params);
  return <NodeDetail nodeKey={decodeURIComponent(key)} />;
}

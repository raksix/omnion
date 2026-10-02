"use client";

/**
 * `/page-templates` — the page template gallery (REQ-063, slice 3).
 *
 * A template is a whole page with sample content, and "use template" creates a *real* draft
 * page whose blocks match it — the REQ's "creates a draft page whose blocks match the template,
 * with the sample content intact". The card therefore has to show what the page will contain
 * before the click, which is why it carries the template's outline read from the block registry
 * rather than a one-word summary.
 *
 * The form asks for a slug and a title and then navigates to the new page's editor. It does not
 * publish anything: what arrives is a draft, and the author's next action is to edit it.
 */
import { useEffect, useMemo, useState } from "react";

import type { BlockRegistry, ContentBlock, Page, PageTemplateSummary, Site } from "@/lib/types";
import { ArrowRight, FileStack, Search } from "lucide-react";
import { useRouter } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  createPageFromTemplate,
  fetchBlockRegistry,
  fetchPageTemplates,
  fetchSites,
} from "@/lib/api";
import { describeTree } from "./block-tree-summary";
import { FALLBACK_LIMITS } from "./block-tree";
import { UseTemplateForm } from "./use-template-form";
import { useContentTenant } from "@/lib/tenant";
import { TenantPicker } from "@/components/tenant-picker";

/** A registry-shaped value for when the registry request failed. */
const NO_REGISTRY: BlockRegistry = {
  version: "0",
  categories: [],
  // An empty registry carries no limits either, and `registryLimits` falls back for it — the
  // library reads blocks, never enforces bounds, so an outage cannot strand a card.
  limits: FALLBACK_LIMITS,
  blocks: [],
};

/** The template gallery. */
export function TemplateGallery() {
  const router = useRouter();
  // A tenant-addressed read has to name its tenant, and the first-run platform account has none of
  // its own. Same argument as the pattern library; see `useContentTenant` for where the tenant
  // comes from and why it is derived rather than typed.
  const tenant = useContentTenant();
  const organizationId = tenant.organizationId ?? undefined;
  const [templates, setTemplates] = useState<PageTemplateSummary[] | null>(null);
  const [registry, setRegistry] = useState<BlockRegistry | null>(null);
  const [sites, setSites] = useState<Site[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState<PageTemplateSummary | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetchBlockRegistry()
      .then((document_) => {
        if (!cancelled) {
          setRegistry(document_);
        }
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    // No tenant resolved yet means no tenant to name; asking anyway is the 400 this replaced.
    if (!organizationId) {
      setTemplates([]);
      setSites([]);
      return () => {
        cancelled = true;
      };
    }
    fetchPageTemplates(organizationId)
      .then((listed) => {
        if (!cancelled) {
          setTemplates(listed);
        }
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setTemplates([]);
          setError(
            cause instanceof ApiError
              ? cause.message
              : "The template gallery could not be loaded.",
          );
        }
      });
    // The gallery is useless without a site to create into, so both arrive together — a form
    // asking for a site id the panel already knows would be a question it cannot answer.
    fetchSites()
      .then((listed) => {
        if (!cancelled) {
          setSites(listed);
        }
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [organizationId]);

  const visible = useMemo(() => {
    if (!templates) {
      return [];
    }
    const needle = query.trim().toLowerCase();
    if (needle === "") {
      return templates;
    }
    return templates.filter(
      (template) =>
        template.name.toLowerCase().includes(needle) ||
        template.key.includes(needle) ||
        (template.description ?? "").toLowerCase().includes(needle),
    );
  }, [templates, query]);

  if (error) {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="The template gallery could not be loaded"
          hint={`${error} The gallery is read with content.blocks.read.`}
        />
      </div>
    );
  }

  if (!templates) {
    return <LoadingTable columns={3} />;
  }

  // The one state a member of a tenant cannot reach: a platform account that holds no tenant has
  // no gallery to be empty. Saying "no templates yet" would describe a library never asked.
  if (tenant.status === "unresolved") {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="No organization to read the gallery from"
          hint="Templates belong to an organization, and this account is not in one. Open a site, or ask an owner for access to a tenant."
        />
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      <div className="flex flex-wrap items-center justify-between gap-3 rounded-xl border border-line bg-surface px-4 py-3">
        <div className="flex items-baseline gap-2">
          <FileStack className="size-4 text-muted" aria-hidden />
          <h2 className="text-[13.5px] font-medium">Page templates</h2>
          <span className="text-[12px] text-muted">
            {templates.length} starting point{templates.length === 1 ? "" : "s"}
          </span>
        </div>
        <div className="flex flex-wrap items-center gap-3">
          <TenantPicker
            organizations={tenant.organizations}
            organizationId={tenant.organizationId}
            onSelect={tenant.selectOrganization}
            testId="page-templates"
          />
          <label className="flex items-center gap-2">
          <span className="sr-only">Search the template gallery</span>
          <Search className="size-3.5 text-muted" aria-hidden />
          <input
            id="template-search"
            name="template-search"
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="Search templates…"
            className="w-52 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
          </label>
        </div>
      </div>

      {visible.length === 0 ? (
        <div className="rounded-xl border border-line bg-surface">
          <EmptyState
            title={templates.length === 0 ? "No templates yet" : "Nothing matches that search"}
            hint={
              templates.length === 0
                ? "The platform's own starting points are seeded on the first read of this gallery."
                : "Clear the search to see every template."
            }
          />
        </div>
      ) : (
        <ul className="grid gap-3 sm:grid-cols-2 xl:grid-cols-3">
          {visible.map((template) => (
            <li
              key={template.id}
              data-template-card={template.key}
              className="flex flex-col gap-2 rounded-xl border border-line bg-surface p-3.5"
            >
              <div className="flex items-baseline gap-2">
                <span className="text-[13px] font-medium">{template.name}</span>
                <code className="font-mono text-[11px] text-muted">{template.key}</code>
                {template.is_system ? (
                  <span className="rounded-full bg-quiet-soft px-1.5 py-0.5 text-[10.5px] text-muted">
                    ships with the platform
                  </span>
                ) : null}
              </div>
              {template.description ? (
                <p className="text-[12px] text-muted">{template.description}</p>
              ) : null}
              <p className="text-[11.5px] text-muted">
                {template.block_count} block{template.block_count === 1 ? "" : "s"} ·{" "}
                {template.page_type} · sample content included
              </p>
              <p className="line-clamp-2 font-mono text-[11px] text-muted/90">
                {describeTree(
                  template.blocks as ContentBlock[],
                  registry ?? NO_REGISTRY,
                  5,
                )}
              </p>
              <button
                type="button"
                data-action={`use-template-${template.key}`}
                onClick={() => setActive(template)}
                className="mt-auto inline-flex w-fit cursor-pointer items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] font-medium transition hover:bg-canvas"
              >
                Use template
                <ArrowRight className="size-3.5" aria-hidden />
              </button>
            </li>
          ))}
        </ul>
      )}

      {active ? (
        <UseTemplateForm
          template={active}
          sites={sites}
          onCancel={() => setActive(null)}
          onCreated={(page: Page) => {
            router.push(`/pages/${page.id}/edit`);
          }}
        />
      ) : null}
    </div>
  );
}

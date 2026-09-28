"use client";

/**
 * The "use template" form (REQ-063, slice 3).
 *
 * It asks for the two things a page cannot be created without — a slug and a title — and one
 * thing the author has to choose: which site the page belongs to. The template's blocks are the
 * server's business; the form never assembles a block tree, because the whole point of the
 * template is that the page is created *through the same store path* as any other page and is
 * therefore a real page with a real revision history.
 *
 * The form never publishes. What it creates is a draft, and it says so, because a button that
 * says "Create page" on a screen full of templates is otherwise read as "publish".
 */
import { useState } from "react";

import type { Page, PageTemplateSummary, Site } from "@/lib/types";
import { Check, X } from "lucide-react";

import { ApiError, createPageFromTemplate } from "@/lib/api";
import { keyFromName } from "./block-tree-summary";

type Props = {
  /** The template the page is created from. */
  template: PageTemplateSummary;
  /** The sites the account may write to. */
  sites: Site[];
  /** Close the form. */
  onCancel: () => void;
  /** The page was created. */
  onCreated: (page: Page) => void;
};

/** Create one page from one template. */
export function UseTemplateForm({ template, sites, onCancel, onCreated }: Props) {
  const [title, setTitle] = useState(template.name);
  const [slug, setSlug] = useState(keyFromName(template.name));
  const [slugTouched, setSlugTouched] = useState(false);
  const [siteId, setSiteId] = useState(sites[0]?.id ?? "");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const effectiveSlug = slugTouched ? slug : keyFromName(title);
  const titleError = title.trim() === "" ? "A title is required." : null;
  const slugError =
    effectiveSlug === ""
      ? "A slug is required."
      : !/^[a-z0-9][a-z0-9-]*$/.test(effectiveSlug)
        ? "Use lowercase letters, digits and dashes."
        : null;
  const siteError = siteId === "" ? "Choose a site." : null;
  const ready = titleError === null && slugError === null && siteError === null && !busy;

  const onCreate = async () => {
    if (!ready) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const page = await createPageFromTemplate({
        siteId,
        templateId: template.id,
        slug: effectiveSlug,
        title: title.trim(),
      });
      onCreated(page);
    } catch (cause: unknown) {
      setError(
        cause instanceof ApiError
          ? cause.message
          : "The page could not be created from the template.",
      );
      setBusy(false);
    }
  };

  return (
    <section
      data-template-form={template.key}
      className="flex flex-col gap-3 rounded-xl border border-accent/40 bg-surface p-4"
    >
      <header className="flex items-baseline justify-between gap-3">
        <h3 className="text-[13.5px] font-medium">
          New page from {template.name}
        </h3>
        <button
          type="button"
          onClick={onCancel}
          aria-label="Close the template form"
          className="cursor-pointer rounded-lg border border-line p-1 transition hover:bg-canvas"
        >
          <X className="size-3.5" aria-hidden />
        </button>
      </header>

      <p className="text-[12px] text-muted">
        The page is created as a <strong>draft</strong> with {template.block_count} block
        {template.block_count === 1 ? "" : "s"} and the template&apos;s sample content. Nothing
        is published — you will land in the editor.
      </p>

      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex flex-col gap-1">
          <span className="text-[12px] font-medium">Title</span>
          <input
            id="template-title"
            name="template-title"
            value={title}
            onChange={(event) => setTitle(event.target.value)}
            aria-invalid={titleError !== null}
            aria-describedby={titleError ? "template-title-error" : undefined}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
          {titleError ? (
            <span id="template-title-error" className="text-[11.5px] text-caution">
              {titleError}
            </span>
          ) : null}
        </label>

        <label className="flex flex-col gap-1">
          <span className="text-[12px] font-medium">Slug</span>
          <input
            id="template-slug"
            name="template-slug"
            value={effectiveSlug}
            onChange={(event) => {
              setSlugTouched(true);
              setSlug(event.target.value);
            }}
            aria-invalid={slugError !== null}
            aria-describedby={slugError ? "template-slug-error" : "template-slug-hint"}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 font-mono text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
          {slugError ? (
            <span id="template-slug-error" className="text-[11.5px] text-caution">
              {slugError}
            </span>
          ) : (
            <span id="template-slug-hint" className="text-[11.5px] text-muted">
              The page&apos;s address. It has to be free on the site.
            </span>
          )}
        </label>

        <label className="flex flex-col gap-1 sm:col-span-2">
          <span className="text-[12px] font-medium">Site</span>
          <select
            id="template-site"
            name="template-site"
            value={siteId}
            onChange={(event) => setSiteId(event.target.value)}
            aria-invalid={siteError !== null}
            aria-describedby={siteError ? "template-site-error" : undefined}
            className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
          >
            <option value="">Choose a site…</option>
            {sites.map((site) => (
              <option key={site.id} value={site.id}>
                {site.name}
              </option>
            ))}
          </select>
          {siteError ? (
            <span id="template-site-error" className="text-[11.5px] text-caution">
              {siteError}
            </span>
          ) : sites.length === 0 ? (
            <span className="text-[11.5px] text-muted">
              No site is in scope for this account, so a page cannot be created. Create a site
              first.
            </span>
          ) : null}
        </label>
      </div>

      {error ? (
        <p
          role="alert"
          data-template-error
          className="rounded-lg border border-caution/40 bg-caution-soft px-2.5 py-2 text-[12px] text-caution"
        >
          {error}
        </p>
      ) : null}

      <div className="flex items-center gap-2">
        <button
          type="button"
          data-action="create-from-template"
          disabled={!ready}
          onClick={() => {
            onCreate();
          }}
          className="inline-flex cursor-pointer items-center gap-1.5 rounded-lg bg-accent px-2.5 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:cursor-not-allowed disabled:opacity-50"
        >
          <Check className="size-3.5" aria-hidden />
          {busy ? "Creating…" : "Create draft page"}
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="cursor-pointer rounded-lg border border-line px-2.5 py-1.5 text-[12.5px] transition hover:bg-canvas"
        >
          Cancel
        </button>
      </div>
    </section>
  );
}

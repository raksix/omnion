"use client";

/**
 * `/pages/<id>/preview` — the front-end preview frame with inline editing (REQ-063, slice 2).
 *
 * The frame draws the page the way the public renderer draws it, from the tree the *server*
 * filtered for the chosen viewport. The alternative — filtering in the browser, or hiding a
 * desktop-only block with CSS — shows an author a page that a phone will never receive, which is
 * the "preview lies" failure the REQ names. So the viewport switch is a round trip and the
 * server owns the answer.
 *
 * Inline editing writes a **draft revision and nothing else**. That is the whole rule of this
 * screen, and it is enforced in three places rather than one: there is no publish control here,
 * the save button calls the same `PATCH /pages/{id}` the editor's *Save draft* calls, and the
 * banner states the live revision number so the author can see it has not moved. A frame that
 * could publish would let an author put a half-finished sentence in front of visitors by
 * clicking in a paragraph.
 *
 * The frame renders with the editor's own `BlockCanvas` in `render` mode — the same component
 * the editor's centre pane uses — so "what I see in the editor" and "what I see in the frame"
 * cannot be two different renderers that drift apart.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import type { BlockIssue, BlockRegistry, ContentBlock } from "@omnion/types";
import {
  Check,
  Eye,
  Monitor,
  Pencil,
  RotateCcw,
  Save,
  Smartphone,
  TriangleAlert,
  X,
} from "lucide-react";
import Link from "next/link";
import { useParams, useRouter } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { ApiError, fetchBlockRegistry, fetchPagePreview, updatePage } from "@/lib/api";
import type { PagePreview } from "@/lib/types";
import { BlockCanvas } from "@/features/blocks/block-canvas";
import { blockLabel, definitionFor } from "@/features/blocks/block-library";
import { blockAt, setProp, walk } from "@/features/blocks/block-tree";

/** The two screens the REQ asks the frame to switch between. */
const VIEWPORTS = [
  { key: "desktop" as const, label: "Desktop", icon: Monitor },
  { key: "mobile" as const, label: "Phone", icon: Smartphone },
];

/** The prop an inline edit writes into, per block type. `null` means "not editable in place". */
const INLINE_PROP: Record<string, string | null> = {
  heading: "text",
  text: "text",
  testimonial: "quote",
  cta: "body",
  raw_html: "html",
};

/**
 * The blocks the frame can edit in place, as id → prop key.
 *
 * Only the plain-text props are editable, and that is a decision about what "inline" means
 * rather than a limitation: a `gallery` is a list of media ids and a `pricing_table` is
 * `|`-joined rows, neither of which a paragraph of caret-and-typing can express. Those blocks
 * are still *shown* and still *selectable* — the frame says where to edit them instead of
 * pretending the click did nothing.
 */
function editableProps(blocks: ContentBlock[]): Map<string, string> {
  const editable = new Map<string, string>();
  for (const { block } of walk(blocks)) {
    const key = INLINE_PROP[block.type];
    if (key) {
      editable.set(block.id, key);
    }
  }
  return editable;
}

/** One toast, dismissed by the screen itself after a few seconds. */
type Toast = { text: string; tone: "ok" | "error" };

/** The preview frame of one page. */
export function BlockPreview() {
  const params = useParams<{ id: string }>();
  const router = useRouter();
  const pageId = params.id;

  const [registry, setRegistry] = useState<BlockRegistry | null>(null);
  const [preview, setPreview] = useState<PagePreview | null>(null);
  const [viewport, setViewport] = useState<"desktop" | "mobile">("desktop");
  const [editing, setEditing] = useState(false);
  // The working copy the frame draws. It starts as the server's answer and is then owned by the
  // author: a save pushes it to the API and the answer replaces it, so what is drawn is always
  // either the saved tree or the author's unsaved edit — never a mixture.
  const [blocks, setBlocks] = useState<ContentBlock[]>([]);
  const [dirty, setDirty] = useState(false);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [toast, setToast] = useState<Toast | null>(null);
  const toastTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const leaving = useRef(false);

  const say = useCallback((next: Toast) => {
    setToast(next);
    if (toastTimer.current) {
      clearTimeout(toastTimer.current);
    }
    toastTimer.current = setTimeout(() => setToast(null), 4500);
  }, []);

  // The registry is the same document for every page, so it is fetched once per screen.
  useEffect(() => {
    let cancelled = false;
    fetchBlockRegistry()
      .then((document_) => {
        if (!cancelled) {
          setRegistry(document_);
        }
      })
      .catch(() => {
        if (!cancelled) {
          setError("The block registry could not be loaded, so the frame cannot draw the page.");
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  // The frame's payload. It is re-read whenever the screen changes, because the whole point of
  // the frame is that it is the *server's* render of the draft rather than a cached tree.
  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setError(null);
    fetchPagePreview(pageId, viewport)
      .then((payload) => {
        if (cancelled) {
          return;
        }
        setPreview(payload);
        setBlocks(
          (Array.isArray(payload.visible_blocks) ? payload.visible_blocks : []) as ContentBlock[],
        );
        // A fresh server answer is the truth about what is saved, so the dirty flag goes with
        // it. Keeping "unsaved" true after a successful save is how a frame teaches an author
        // their work is not there.
        setDirty(false);
        setLoading(false);
      })
      .catch((cause: unknown) => {
        if (cancelled) {
          return;
        }
        setError(
          cause instanceof ApiError ? cause.message : "The page could not be loaded.",
        );
        setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [pageId, viewport]);

  // Leaving with unsaved inline edits must ask, or the author loses a paragraph to a mis-click.
  // `beforeunload` covers the tab; the link handler covers the panel's own navigation, which is
  // the one an author actually takes.
  useEffect(() => {
    if (!dirty) {
      return;
    }
    const onBeforeUnload = (event: BeforeUnloadEvent) => {
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", onBeforeUnload);
    return () => window.removeEventListener("beforeunload", onBeforeUnload);
  }, [dirty]);

  const goBack = useCallback(
    (event: React.MouseEvent) => {
      if (dirty && !window.confirm("This page has inline edits that were not saved. Leave anyway?")) {
        event.preventDefault();
        return;
      }
      leaving.current = true;
    },
    [dirty],
  );

  const visible = useMemo(
    () => (Array.isArray(preview?.visible_blocks) ? (preview.visible_blocks as ContentBlock[]) : []),
    [preview],
  );
  const editable = useMemo(() => editableProps(visible), [visible]);
  const issuesByBlock = useMemo(() => {
    const map = new Map<string, BlockIssue[]>();
    for (const issue of preview?.issues ?? []) {
      const existing = map.get(issue.block_id);
      if (existing) {
        existing.push(issue);
      } else {
        map.set(issue.block_id, [issue]);
      }
    }
    return map;
  }, [preview]);

  /**
   * Write an inline edit into the working tree.
   *
   * The path is found by id rather than by the click's position: the author is editing the text
   * they are looking at, and the path of "the block under the caret" is bookkeeping the browser
   * should not be doing. `walk` hands back both, and the id is the one that cannot be wrong.
   */
  const writeInline = useCallback((blockId: string, value: string) => {
    setBlocks((current) => {
      const entry = walk(current).find(({ block }) => block.id === blockId);
      if (!entry) {
        return current;
      }
      const key = INLINE_PROP[entry.block.type];
      if (!key) {
        return current;
      }
      return setProp(current, entry.path, key, value);
    });
    setDirty(true);
  }, []);

  const save = async () => {
    if (saving) {
      return;
    }
    setSaving(true);
    setError(null);
    try {
      // The same call the editor's *Save draft* makes: one new draft revision, never a publish.
      const updated = await updatePage(pageId, { blocks });
      setDirty(false);
      // The answer names the revision the server actually wrote. Reading the number back from
      // the response is what makes the toast a fact rather than an incrementing local counter
      // that would claim revision 9 when the server wrote revision 3.
      const revisionNo = updated.draft?.revision_no ?? null;
      const published = updated.published?.revision_no ?? null;
      say({
        tone: "ok",
        text:
          revisionNo === null
            ? "Saved."
            : `Saved as draft revision ${revisionNo}.` +
              (published === null ? "" : ` Visitors still see revision ${published}.`),
      });
      // Re-read so the frame draws the server's own tree (post-sanitisation) rather than the
      // bytes that were sent.
      const payload = await fetchPagePreview(pageId, viewport);
      setPreview(payload);
      setBlocks(
        (Array.isArray(payload.visible_blocks) ? payload.visible_blocks : []) as ContentBlock[],
      );
    } catch (cause: unknown) {
      const message =
        cause instanceof ApiError ? cause.message : "The page could not be saved.";
      setError(message);
      // The local state is kept on purpose: the author's text is still in the frame, and a
      // silent loss would be worse than a visible failure.
      say({ tone: "error", text: message });
    } finally {
      setSaving(false);
    }
  };

  const reload = async () => {
    if (dirty && !window.confirm("Discard the inline edits and read the saved page again?")) {
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const payload = await fetchPagePreview(pageId, viewport);
      setPreview(payload);
      setBlocks(
        (Array.isArray(payload.visible_blocks) ? payload.visible_blocks : []) as ContentBlock[],
      );
      setDirty(false);
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The page could not be re-read.");
    } finally {
      setLoading(false);
    }
  };

  if (error && !registry) {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState
          title="The preview could not be opened"
          hint={error}
          action={
            <Link
              href="/pages"
              onClick={goBack}
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              Back to pages
            </Link>
          }
        />
      </div>
    );
  }

  if (loading && !preview) {
    return (
      <div className="rounded-xl border border-line bg-surface px-4 py-8" data-block-preview-loading>
        <p className="text-[12.5px] text-muted">Loading the page as the renderer sees it…</p>
      </div>
    );
  }

  if (!registry || !preview) {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <EmptyState title="Nothing to preview" hint="This page has no working draft." />
      </div>
    );
  }

  const hiddenCount = preview.block_count - preview.visible_count;

  return (
    <div className="flex flex-col gap-4" data-block-preview>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-baseline gap-2">
          <h2 className="text-[15px] font-medium">{preview.title}</h2>
          <span className="font-mono text-[12px] text-muted">/{preview.slug}</span>
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Link
            href={`/pages/${pageId}/edit`}
            onClick={goBack}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Open in the editor
          </Link>
          <Link
            href="/pages"
            onClick={goBack}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            All pages
          </Link>
        </div>
      </div>

      {/* The banner: what the frame is, and the two things it must never be confused with —
          a live page, and a lost edit. */}
      <div
        data-block-preview-banner
        data-block-preview-draft={preview.revision_no}
        data-block-preview-live={preview.published_revision_no ?? ""}
        className="flex flex-wrap items-center gap-x-4 gap-y-1 rounded-xl border border-caution/40 bg-caution-soft px-4 py-2.5 text-[12px] text-caution"
      >
        <span className="inline-flex items-center gap-1.5 font-medium">
          <TriangleAlert className="size-3.5" aria-hidden />
          Draft preview
        </span>
        <span>draft v{preview.revision_no}</span>
        <span>
          {preview.published_revision_no === null
            ? "not published"
            : `visitors see v${preview.published_revision_no}`}
        </span>
        <span className="ml-auto text-muted">
          Nothing here is published — every save appends a draft revision.
        </span>
      </div>

      {error ? (
        <p
          role="alert"
          data-block-preview-error
          className="rounded-xl border border-accent/40 bg-accent-soft px-4 py-3 text-[12.5px] text-accent-strong"
        >
          {error}
        </p>
      ) : null}

      {/* The overlay toolbar: the screen switch, the inline toggle, save and reload. */}
      <div className="flex flex-wrap items-center gap-2">
        <div
          role="group"
          aria-label="Screen size"
          data-block-preview-viewports
          className="flex items-center gap-1 rounded-lg border border-line p-0.5"
        >
          {VIEWPORTS.map(({ key, label, icon: Icon }) => (
            <button
              key={key}
              type="button"
              data-block-preview-viewport={key}
              onClick={() => setViewport(key)}
              aria-pressed={viewport === key}
              className={`flex items-center gap-1.5 rounded-md px-2.5 py-1 text-[12px] transition ${
                viewport === key ? "bg-accent-soft text-accent-strong" : "text-muted hover:text-ink"
              }`}
            >
              <Icon className="size-3" aria-hidden />
              {label}
            </button>
          ))}
        </div>

        <button
          type="button"
          data-block-preview-toggle-edit
          onClick={() => setEditing((value) => !value)}
          aria-pressed={editing}
          className={`flex items-center gap-1.5 rounded-lg border px-3 py-1.5 text-[12.5px] transition ${
            editing
              ? "border-accent bg-accent-soft text-accent-strong"
              : "border-line hover:bg-canvas"
          }`}
        >
          {editing ? <Check className="size-3.5" aria-hidden /> : <Pencil className="size-3.5" aria-hidden />}
          {editing ? "Editing on" : "Edit inline"}
        </button>

        <button
          type="button"
          data-block-preview-save
          onClick={save}
          disabled={saving || !dirty}
          title={dirty ? "Append a draft revision" : "No unsaved inline edits"}
          className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
        >
          <Save className="size-3.5" aria-hidden />
          {saving ? "Saving…" : "Save draft"}
        </button>

        <button
          type="button"
          data-block-preview-reload
          onClick={reload}
          className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
        >
          <RotateCcw className="size-3.5" aria-hidden />
          Reload
        </button>

        {dirty ? (
          <span
            data-block-preview-dirty
            className="inline-flex items-center gap-1.5 text-[12px] text-caution"
          >
            <span className="size-1.5 rounded-full bg-caution" aria-hidden />
            Unsaved inline edits
          </span>
        ) : null}
      </div>

      {/* The frame. The narrow screen is not a CSS trick: the *server* already dropped the
          blocks hidden from a phone, so what is drawn here is the phone payload. */}
      <div
        data-block-preview-frame
        data-block-preview-viewport-active={viewport}
        className={`rounded-xl border border-line bg-surface transition ${
          viewport === "mobile" ? "mx-auto w-full max-w-[390px]" : "w-full"
        }`}
      >
        {visible.length === 0 ? (
          <div className="px-4 py-6">
            <EmptyState
              title={
                preview.block_count === 0
                  ? "This page has no blocks yet"
                  : "Nothing renders on this screen"
              }
              hint={
                preview.block_count === 0
                  ? "Build the page in the editor, or start from a page template."
                  : `All ${preview.block_count} blocks on this page are hidden from the ${viewport === "mobile" ? "phone" : "desktop"} render. Clear a block's “hide on” setting in the editor to see it here.`
              }
              action={
                <Link
                  href={`/pages/${pageId}/edit`}
                  onClick={goBack}
                  className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
                >
                  Open in the editor
                </Link>
              }
            />
          </div>
        ) : (
          <div className="p-4">
            <BlockCanvas
              registry={registry}
              blocks={blocks}
              mode={editing ? "edit" : "render"}
              issues={issuesByBlock}
              editable={editing ? editable : undefined}
              onInlineEdit={editing ? writeInline : undefined}
            />
          </div>
        )}
      </div>

      {/* The bottom line: what is in the page, what this screen renders, and the toast the
          REQ asks the save to produce. */}
      <div
        data-block-preview-status
        data-block-preview-block-count={preview.block_count}
        data-block-preview-visible-count={preview.visible_count}
        data-block-preview-dirty={dirty ? "true" : "false"}
        className="flex flex-wrap items-center gap-x-4 gap-y-1 rounded-xl border border-line bg-surface px-4 py-2.5 text-[12px] text-muted"
      >
        <span className="flex items-center gap-1.5">
          <Eye className="size-3" aria-hidden />
          {preview.visible_count} of {preview.block_count} blocks render on {viewport}
          {hiddenCount > 0 ? ` · ${hiddenCount} hidden here` : ""}
        </span>
        {preview.can_publish ? (
          <span className="text-positive">Draft is whole enough to publish</span>
        ) : (
          <span className="text-caution">
            {(preview.issues ?? []).filter((issue) => issue.severity === "error").length} blocking
            issue(s)
          </span>
        )}
        {toast ? (
          <span
            role="status"
            data-block-preview-toast
            data-tone={toast.tone}
            className={`ml-auto inline-flex items-center gap-1.5 ${
              toast.tone === "ok" ? "text-positive" : "text-accent-strong"
            }`}
          >
            {toast.tone === "ok" ? <Check className="size-3" aria-hidden /> : <X className="size-3" aria-hidden />}
            {toast.text}
          </span>
        ) : null}
        <button
          type="button"
          onClick={() => router.push("/pages")}
          onClickCapture={goBack}
          className="rounded-md border border-line px-2 py-0.5 text-[11.5px] transition hover:bg-canvas"
        >
          Close
        </button>
      </div>
    </div>
  );
}

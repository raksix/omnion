"use client";

/**
 * `/pages/<id>/revisions` — the history of a page, and the compare between two of its
 * revisions (REQ-063, slice 2).
 *
 * The compare is the reason this screen exists. A list of revision numbers answers "what
 * exists"; it does not answer "what did the author change", which is the question an author
 * opens this screen with. So the screen loads the compare by default rather than waiting for a
 * base to be picked — the API defaults the base to the previous revision, and an author who
 * wants a different one names it in the picker.
 *
 * What a row says is the server's decision (`crates/content/src/blockdiff.rs`), not this
 * component's: the panel renders added / removed / changed / moved with prop-level detail, and
 * it does not compute a diff of its own. Two implementations of "what changed" is how a panel
 * and a server start disagreeing about a page.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import type { BlockDiffEntry, PropChange, Revision, RevisionDiff } from "@/lib/types";
import {
  ArrowRight,
  GitCompareArrows,
  History,
  Minus,
  Pencil,
  Plus,
  RotateCcw,
  TriangleAlert,
} from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { ApiError, fetchPage, fetchRevisionDiff, fetchRevisions } from "@/lib/api";

/** How a row is presented: the icon, the wording and the tone, all keyed by the server's verb. */
const CHANGE_TONE: Record<
  BlockDiffEntry["change"],
  { label: string; icon: typeof Plus; className: string }
> = {
  added: {
    label: "Added",
    icon: Plus,
    className: "text-emerald-700 dark:text-emerald-400",
  },
  removed: {
    label: "Removed",
    icon: Minus,
    className: "text-red-700 dark:text-red-400",
  },
  changed: {
    label: "Changed",
    icon: Pencil,
    className: "text-amber-700 dark:text-amber-400",
  },
  moved: {
    label: "Moved",
    icon: ArrowRight,
    className: "text-sky-700 dark:text-sky-400",
  },
  unchanged: {
    label: "Unchanged",
    icon: Minus,
    className: "text-muted",
  },
};

export function RevisionHistoryView({ pageId }: { pageId: string }) {
  const [page, setPage] = useState<{ title: string; slug: string } | null>(null);
  const [revisions, setRevisions] = useState<Revision[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  // The revision being read, and the one it is measured from. `against` starts empty because
  // "no choice" is a real and common answer: the API picks the previous revision.
  const [comparedId, setComparedId] = useState<string | null>(null);
  const [againstId, setAgainstId] = useState<string>("");
  const [diff, setDiff] = useState<RevisionDiff | null>(null);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [diffLoading, setDiffLoading] = useState(false);

  useEffect(() => {
    let cancelled = false;
    Promise.all([fetchPage(pageId), fetchRevisions(pageId)])
      .then(([loadedPage, loadedRevisions]) => {
        if (cancelled) {
          return;
        }
        setPage({ title: loadedPage.draft?.title ?? loadedPage.published?.title ?? loadedPage.slug, slug: loadedPage.slug });
        setRevisions(loadedRevisions);
        // Open the newest revision that is not the first one: the first has nothing before it,
        // so comparing it would immediately report an error the author did not cause.
        const openable = loadedRevisions.find((entry) => entry.revision_no > 1) ?? loadedRevisions[0];
        setComparedId(openable?.id ?? null);
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(
            cause instanceof ApiError ? cause.message : "The revision history could not be loaded.",
          );
        }
      })
      .finally(() => {
        if (!cancelled) {
          setLoading(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, [pageId]);

  const loadDiff = useCallback(
    (revisionId: string, against: string) => {
      setDiffLoading(true);
      setDiffError(null);
      fetchRevisionDiff(pageId, revisionId, against === "" ? undefined : against)
        .then(setDiff)
        .catch((cause: unknown) => {
          setDiff(null);
          setDiffError(
            cause instanceof ApiError
              ? cause.message
              : "The two revisions could not be compared.",
          );
        })
        .finally(() => setDiffLoading(false));
    },
    [pageId],
  );

  useEffect(() => {
    if (comparedId) {
      loadDiff(comparedId, againstId);
    }
  }, [comparedId, againstId, loadDiff]);

  const compared = useMemo(
    () => revisions?.find((entry) => entry.id === comparedId) ?? null,
    [revisions, comparedId],
  );

  if (loading) {
    return <LoadingRows label="Loading the revision history" />;
  }
  if (error) {
    return <EmptyState title="This history could not be loaded" hint={error} />;
  }
  if (!revisions || revisions.length === 0) {
    return (
      <EmptyState
        title="No revisions yet"
        hint="A page always carries at least one revision. Save a change and the history starts here."
      />
    );
  }

  return (
    <div className="flex flex-col gap-6">
      <header className="flex flex-wrap items-baseline justify-between gap-2">
        <div>
          <h2 className="text-[15px] font-medium">{page?.title ?? "Revision history"}</h2>
          <p className="text-[12.5px] text-muted">
            Every save appends a revision and publishing freezes one. Comparing two of them is
            how you see what actually changed.
          </p>
        </div>
        <Link
          href={`/pages/${encodeURIComponent(pageId)}/edit`}
          className="text-[12.5px] text-muted underline underline-offset-4 hover:text-ink"
        >
          Open the editor
        </Link>
      </header>

      <div className="grid gap-6 lg:grid-cols-[minmax(0,20rem)_minmax(0,1fr)]">
        <RevisionList
          revisions={revisions}
          comparedId={comparedId}
          onCompare={setComparedId}
        />

        <section className="flex flex-col gap-4" aria-label="Compare revisions">
          <div className="flex flex-wrap items-end gap-3">
            <label className="flex flex-col gap-1" htmlFor="revision-against">
              <span className="text-[11px] tracking-wide text-muted uppercase">Compare with</span>
              <select
                id="revision-against"
                data-revision-against
                className="rounded-md border border-line bg-surface px-2 py-1.5 text-[12.5px]"
                value={againstId}
                onChange={(event) => setAgainstId(event.target.value)}
              >
                <option value="">The previous revision</option>
                {revisions
                  .filter((entry) => entry.id !== comparedId)
                  .map((entry) => (
                    <option key={entry.id} value={entry.id}>
                      v{entry.revision_no} · {entry.state}
                    </option>
                  ))}
              </select>
            </label>
            {compared && (
              <p className="text-[12.5px] text-muted">
                Reading <strong className="font-medium text-ink">v{compared.revision_no}</strong>{" "}
                ({compared.state})
              </p>
            )}
          </div>

          {diffLoading ? (
            <LoadingRows label="Comparing the two revisions" />
          ) : diffError ? (
            <EmptyState title="Nothing to compare against" hint={diffError} />
          ) : diff ? (
            <DiffResult diff={diff} />
          ) : null}
        </section>
      </div>
    </div>
  );
}

/**
 * Skeleton rows in the shape the real list takes.
 *
 * `LoadingTable` is the wrong tool here and using it anyway is how a screen ends up with a
 * table of bars standing in for a list of revisions: the column count would be a guess and the
 * browser's table semantics would claim this is tabular data when it is a list of buttons.
 */
function LoadingRows({ label }: { label: string }) {
  return (
    <div className="flex flex-col gap-2" aria-busy="true" aria-label={label}>
      {Array.from({ length: 3 }, (_, index) => (
        <div key={index} className="flex flex-col gap-1.5 rounded-md border border-line p-3">
          <span className="h-3 w-32 animate-pulse rounded bg-quiet-soft" />
          <span className="h-3 w-24 animate-pulse rounded bg-quiet-soft" />
        </div>
      ))}
    </div>
  );
}

/** The history itself, newest first, with the revision under the compare highlighted. */
function RevisionList({
  revisions,
  comparedId,
  onCompare,
}: {
  revisions: Revision[];
  comparedId: string | null;
  onCompare: (id: string) => void;
}) {
  return (
    <ol className="flex flex-col gap-1" data-revision-list>
      {revisions.map((revision) => {
        const selected = revision.id === comparedId;
        return (
          <li key={revision.id}>
            <button
              type="button"
              data-revision-row
              data-revision-no={revision.revision_no}
              aria-current={selected ? "true" : undefined}
              onClick={() => onCompare(revision.id)}
              className={[
                "flex w-full flex-col gap-0.5 rounded-md border px-3 py-2 text-left transition",
                selected
                  ? "border-ink/30 bg-accent/10"
                  : "border-transparent hover:border-line hover:bg-surface",
              ].join(" ")}
            >
              <span className="flex items-center gap-2 text-[12.5px] font-medium">
                <History aria-hidden className="size-3.5 text-muted" />
                v{revision.revision_no}
                <span className="text-[11px] font-normal text-muted">{revision.state}</span>
                {Array.isArray(revision.blocks) && revision.blocks.length > 0 ? (
                  <span className="text-[11px] font-normal text-muted">
                    {revision.blocks.length} blocks
                  </span>
                ) : (
                  <span className="text-[11px] font-normal text-muted">body</span>
                )}
              </span>
              <span className="text-[12px] text-muted">
                {new Date(revision.created_at).toLocaleString()}
              </span>
            </button>
          </li>
        );
      })}
    </ol>
  );
}

/** The compare, as a list of rows and a body paragraph when there is one. */
function DiffResult({ diff }: { diff: RevisionDiff }) {
  const { blocks, body, base, compared } = diff;
  const nothing =
    blocks.entries.length === 0 && !body.changed;

  return (
    <div className="flex flex-col gap-3" data-revision-diff>
      <p className="text-[12.5px] text-muted">
        v{base.revision_no} <ArrowRight aria-hidden className="inline size-3" />{" "}
        v{compared.revision_no}
      </p>

      {blocks.has_removals && (
        <p
          data-diff-removals-warning
          className="flex items-start gap-2 rounded-md border border-amber-500/40 bg-amber-500/10 px-3 py-2 text-[12.5px]"
        >
          <TriangleAlert aria-hidden className="mt-0.5 size-4 shrink-0 text-amber-600" />
          <span>
            {blocks.removed} block{blocks.removed === 1 ? "" : "s"} left the page between these two
            revisions. Restoring the older revision brings them back.
          </span>
        </p>
      )}

      <div className="flex flex-wrap gap-2 text-[12px]">
        {(
          [
            ["added", blocks.added],
            ["changed", blocks.changed],
            ["moved", blocks.moved],
            ["removed", blocks.removed],
          ] as const
        ).map(([change, count]) => (
          <span
            key={change}
            data-diff-count={change}
            className={[
              "rounded-full border border-line px-2 py-0.5",
              count === 0 ? "text-muted" : CHANGE_TONE[change].className,
            ].join(" ")}
          >
            {count} {CHANGE_TONE[change].label.toLowerCase()}
          </span>
        ))}
      </div>

      {body.changed && (
        <div className="flex flex-col gap-2 rounded-md border border-line p-3">
          <h3 className="text-[12.5px] font-medium">Body text</h3>
          <BodyLine side="before" text={body.before} />
          <BodyLine side="after" text={body.after} />
        </div>
      )}

      {nothing ? (
        <EmptyState
          title="These two revisions are identical"
          hint="Nothing was added, changed, moved or removed between them."
        />
      ) : (
        <ul className="flex flex-col gap-2" data-diff-entries>
          {blocks.entries.map((entry) => (
            <DiffRow key={`${entry.change}-${entry.block_id}`} entry={entry} />
          ))}
        </ul>
      )}

      {blocks.entries.length > 0 && (
        <p className="text-[12px] text-muted">
          <RotateCcw aria-hidden className="mr-1 inline size-3" />
          Comparing never changes anything. Restoring is a separate, deliberate action.
        </p>
      )}
    </div>
  );
}

/** One before/after pair of body text, elided so a long page does not bury the rows. */
function BodyLine({ side, text }: { side: "before" | "after"; text: string }) {
  const shown = text.length > 400 ? `${text.slice(0, 400)}…` : text;
  return (
    <p className="text-[12.5px]">
      <span className="mr-2 text-[11px] tracking-wide text-muted uppercase">{side}</span>
      <span
        data-diff-body={side}
        className={
          side === "before"
            ? "text-muted line-through decoration-red-500/40"
            : "text-ink"
        }
      >
        {shown === "" ? <em className="text-muted">empty</em> : shown}
      </span>
    </p>
  );
}

/** One block's row: the verb, the headline, and the props that changed. */
function DiffRow({ entry }: { entry: BlockDiffEntry }) {
  const tone = CHANGE_TONE[entry.change];
  const Icon = tone.icon;
  return (
    <li
      data-diff-entry
      data-change={entry.change}
      data-block-type={entry.block_type}
      className="flex flex-col gap-1.5 rounded-md border border-line p-3"
    >
      <span className="flex flex-wrap items-center gap-2 text-[12.5px]">
        <Icon aria-hidden className={`size-3.5 ${tone.className}`} />
        <span className={tone.className}>{tone.label}</span>
        <span className="font-medium">{entry.label || entry.block_type}</span>
        <span className="text-[11px] text-muted">{entry.block_type}</span>
        {entry.change === "removed" && (entry.removed_count ?? 0) > 1 && (
          <span className="text-[11px] text-muted">
            and {(entry.removed_count ?? 0) - 1} block
            {(entry.removed_count ?? 0) - 1 === 1 ? "" : "s"} inside it
          </span>
        )}
        {entry.change === "moved" && (
          <span className="text-[11px] text-muted">
            {entry.from_path} <ArrowRight aria-hidden className="inline size-3" />{" "}
            {entry.to_path}
          </span>
        )}
      </span>
      {entry.props.map((change) => (
        <PropRow key={change.path} change={change} />
      ))}
    </li>
  );
}

/** One prop's before and after, in words rather than JSON. */
function PropRow({ change }: { change: PropChange }) {
  return (
    <p className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5 text-[12.5px]">
      <span className="font-medium">{change.label ?? change.path}</span>
      <span className="text-[11px] text-muted">{change.path}</span>
      {change.added ? (
        <span className="text-emerald-700 dark:text-emerald-400">was empty, now set</span>
      ) : (
        <span className="text-muted line-through decoration-red-500/40">
          {change.before === "" ? "empty" : change.before}
        </span>
      )}
      {!change.removed && (
        <ArrowRight aria-hidden className="size-3 shrink-0 text-muted" />
      )}
      {!change.added && (
        <span className={change.removed ? "text-red-700 dark:text-red-400" : undefined}>
          {change.after === "" ? "empty" : change.after}
        </span>
      )}
    </p>
  );
}

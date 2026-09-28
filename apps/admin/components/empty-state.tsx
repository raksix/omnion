import type { ReactNode } from "react";

type EmptyStateProps = {
  /** What is missing, in the panel's own words. */
  title: string;
  /** How to get past it. */
  hint?: string;
  /** The action that gets past it, when there is one. */
  action?: ReactNode;
  /**
   * Names this empty state in the DOM, so a QA pass can tell "this panel has nothing" from
   * "this panel failed to load" — two screens that are identical in pixels and opposite in
   * meaning, and indistinguishable to anything that only counts rows.
   */
  testId?: string;
};

/** Shown instead of a table when there is nothing to list. */
export function EmptyState({ title, hint, action, testId }: EmptyStateProps) {
  return (
    <div data-empty-state={testId} className="flex flex-col items-center gap-2 px-6 py-12 text-center">
      <p className="text-[13.5px] font-medium">{title}</p>
      {hint ? <p className="max-w-sm text-[12.5px] text-muted">{hint}</p> : null}
      {action ? <div className="mt-2">{action}</div> : null}
    </div>
  );
}

import type { ReactNode } from "react";

type EmptyStateProps = {
  /** What is missing, in the panel's own words. */
  title: string;
  /** How to get past it. */
  hint?: string;
  /** The action that gets past it, when there is one. */
  action?: ReactNode;
};

/** Shown instead of a table when there is nothing to list. */
export function EmptyState({ title, hint, action }: EmptyStateProps) {
  return (
    <div className="flex flex-col items-center gap-2 px-6 py-12 text-center">
      <p className="text-[13.5px] font-medium">{title}</p>
      {hint ? <p className="max-w-sm text-[12.5px] text-muted">{hint}</p> : null}
      {action ? <div className="mt-2">{action}</div> : null}
    </div>
  );
}

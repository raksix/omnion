"use client";

/**
 * The insert panel and the inspector of the block editor (REQ-063).
 *
 * Both are generated from the registry: the insert panel lists the categories the API declares
 * and the inspector renders one field per prop schema. Neither names a block type, a prop or
 * an option, so the panel and the validator can never disagree about what a block needs.
 *
 * Validation messages come from the API's own dry run rather than from a second copy of the
 * rules in the browser. The client check that exists here is the one thing a server round-trip
 * cannot do: refuse to *send* a value the field itself says is too long, so the author sees it
 * while they type.
 */
import { useMemo, useState } from "react";

import type {
  BlockDefinition,
  BlockIssue,
  BlockPropSchema,
  BlockRegistry,
  ContentBlock,
} from "@omnion/types";
import { AlertTriangle, Plus, Search } from "lucide-react";

import { categoryLabel, definitionFor, propValue } from "./block-library";

// ---------------------------------------------------------------------------------------------
// The insert panel
// ---------------------------------------------------------------------------------------------

type InsertPanelProps = {
  /** The registry the panel lists. */
  registry: BlockRegistry;
  /** Inserting a block. */
  onPick: (definition: BlockDefinition) => void;
  /** Close the panel. */
  onClose: () => void;
};

/** The `+ Block` panel: searchable, grouped by the registry's own category order. */
export function InsertPanel({ registry, onPick, onClose }: InsertPanelProps) {
  const [query, setQuery] = useState("");

  const grouped = useMemo(() => {
    const needle = query.trim().toLowerCase();
    const matches = registry.blocks.filter((entry) => {
      if (needle === "") {
        return true;
      }
      return (
        entry.label.toLowerCase().includes(needle) ||
        entry.key.includes(needle) ||
        entry.description.toLowerCase().includes(needle)
      );
    });
    return registry.categories
      .map((category) => ({
        category,
        entries: matches.filter((entry) => entry.category === category),
      }))
      .filter((group) => group.entries.length > 0);
  }, [registry, query]);

  const total = grouped.reduce((sum, group) => sum + group.entries.length, 0);

  return (
    <div className="rounded-xl border border-line bg-surface" data-block-insert-panel>
      <div className="flex items-center gap-2 border-b border-line px-3 py-2">
        <label className="sr-only" htmlFor="block-search">
          Search block types
        </label>
        <Search className="size-3.5 shrink-0 text-muted" aria-hidden />
        <input
          id="block-search"
          name="block-search"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder="Search blocks…"
          autoComplete="off"
          className="w-full bg-transparent text-[12.5px] outline-none placeholder:text-muted"
        />
        <button
          type="button"
          onClick={onClose}
          className="shrink-0 rounded-md border border-line px-2 py-0.5 text-[11.5px] text-muted transition hover:text-ink"
        >
          Close
        </button>
      </div>
      <div className="max-h-72 overflow-y-auto px-3 py-2">
        {total === 0 ? (
          <p className="py-6 text-center text-[12.5px] text-muted">
            No block type matches “{query}”.
          </p>
        ) : (
          grouped.map((group) => (
            <div key={group.category} className="mb-3 last:mb-0">
              <h3 className="mb-1.5 text-[11px] tracking-wide text-muted uppercase">
                {categoryLabel(group.category)}
              </h3>
              <ul className="flex flex-col gap-1">
                {group.entries.map((entry) => (
                  <li key={entry.key}>
                    <button
                      type="button"
                      data-block-insert-option={entry.key}
                      onClick={() => onPick(entry)}
                      aria-label={`Insert ${entry.label} block`}
                      className="flex w-full cursor-pointer items-start gap-2 rounded-lg border border-line px-2.5 py-2 text-left transition hover:border-accent/40 hover:bg-canvas"
                    >
                      <Plus className="mt-0.5 size-3 shrink-0 text-accent" aria-hidden />
                      <span className="flex min-w-0 flex-col">
                        <span className="text-[12.5px] font-medium">{entry.label}</span>
                        <span className="text-[11.5px] text-muted">{entry.description}</span>
                      </span>
                    </button>
                  </li>
                ))}
              </ul>
            </div>
          ))
        )}
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// The inspector
// ---------------------------------------------------------------------------------------------

type InspectorProps = {
  /** The registry the fields are generated from. */
  registry: BlockRegistry;
  /** The selected block. */
  block: ContentBlock;
  /** Its live issues from the API's dry run. */
  issues: BlockIssue[];
  /** Writing one prop. */
  onChange: (key: string, value: unknown) => void;
  /** The block's manipulation actions (move, duplicate, delete). */
  actions: React.ReactNode;
  /** The breadcrumb that says where the block sits, for a nested selection. */
  breadcrumb: Array<{ label: string; path: number[] }>;
  /** Selecting a step of the breadcrumb. */
  onCrumb: (path: number[]) => void;
};

/** The right pane: the selected block's fields, generated from its schema. */
export function BlockInspector({
  registry,
  block,
  issues,
  onChange,
  actions,
  breadcrumb,
  onCrumb,
}: InspectorProps) {
  const definition = definitionFor(registry, block.type);

  return (
    <div className="flex flex-col gap-3">
      {breadcrumb.length > 1 ? (
        <nav aria-label="Block position" className="flex flex-wrap items-center gap-1 text-[11.5px]">
          {breadcrumb.map((crumb, index) => (
            <span key={crumb.path.join("-")} className="flex items-center gap-1">
              {index > 0 ? (
                <span aria-hidden className="text-muted">
                  /
                </span>
              ) : null}
              <button
                type="button"
                onClick={() => onCrumb(crumb.path)}
                className="rounded px-1 text-muted transition hover:text-ink"
              >
                {crumb.label}
              </button>
            </span>
          ))}
        </nav>
      ) : null}

      <div className="rounded-xl border border-line bg-surface" data-block-inspector>
        <div className="flex items-center justify-between border-b border-line px-3 py-2">
          <h2 className="text-[13px] font-medium">{definition?.label ?? block.type}</h2>
          {definition ? (
            <span className="text-[11px] text-muted">{definition.category}</span>
          ) : null}
        </div>
        <div className="flex flex-col gap-3 px-3 py-3">
          {definition ? (
            definition.props.map((prop) => (
              <PropField
                key={prop.key}
                prop={prop}
                value={propValue(definition, block.props, prop.key)}
                issue={issues.find((entry) => entry.path.endsWith(`.props.${prop.key}`))}
                onChange={(value) => onChange(prop.key, value)}
              />
            ))
          ) : (
            <p className="text-[12.5px] text-caution">
              This platform does not ship a <code className="font-mono">{block.type}</code> block,
              so there is nothing to configure. Saving the page with it will be refused until the
              block is removed.
            </p>
          )}
        </div>
        {actions ? (
          <div className="flex flex-wrap items-center gap-2 border-t border-line px-3 py-2">
            {actions}
          </div>
        ) : null}
      </div>

      {issues.length > 0 ? (
        <ul className="flex flex-col gap-1" aria-label="Block issues" data-block-issues>
          {issues.map((issue) => (
            <li
              key={`${issue.code}-${issue.path}`}
              className={`flex items-start gap-1.5 rounded-lg border px-2.5 py-2 text-[11.5px] ${
                issue.severity === "error"
                  ? "border-accent/40 bg-accent-soft text-accent-strong"
                  : "border-caution/40 bg-caution-soft text-caution"
              }`}
            >
              <AlertTriangle className="mt-0.5 size-3 shrink-0" aria-hidden />
              <span>
                {issue.message}
                <span className="block font-mono opacity-70">{issue.code}</span>
              </span>
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

type PropFieldProps = {
  prop: BlockPropSchema;
  value: unknown;
  issue: BlockIssue | undefined;
  onChange: (value: unknown) => void;
};

/** One field of a block, drawn from its schema entry. */
function PropField({ prop, value, issue, onChange }: PropFieldProps) {
  const fieldId = `block-prop-${prop.key}`;
  const hintId = `${fieldId}-hint`;
  const tooLong =
    typeof value === "string" &&
    typeof prop.maxLength === "number" &&
    value.length > prop.maxLength;

  const describedBy = [tooLong ? hintId : null, issue ? `${fieldId}-issue` : null]
    .filter(Boolean)
    .join(" ");

  const label = (
    <span className="flex items-center justify-between text-[12px] font-medium">
      <span>
        {prop.label}
        {prop.required ? <span className="ml-1 text-accent-strong">*</span> : null}
      </span>
      {prop.required ? (
        <span className="text-[10.5px] font-normal text-muted">required</span>
      ) : null}
    </span>
  );

  const messages = (
    <>
      {tooLong ? (
        <span id={hintId} className="text-[11px] text-accent-strong">
          {prop.label} must stay under {prop.maxLength} characters (it is {value.length}).
        </span>
      ) : null}
      {issue ? (
        <span id={`${fieldId}-issue`} className="text-[11px] text-accent-strong">
          {issue.message}
        </span>
      ) : null}
    </>
  );

  const shared = `w-full rounded-lg border bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15 ${
    tooLong || issue ? "border-accent-strong" : "border-line"
  }`;

  return (
    <label htmlFor={fieldId} className="flex flex-col gap-1">
      {label}
      {prop.type === "enum" ? (
        <select
          id={fieldId}
          name={fieldId}
          value={typeof value === "string" ? value : (prop.enum?.[0] ?? "")}
          onChange={(event) => onChange(event.target.value)}
          aria-describedby={describedBy || undefined}
          className={shared}
        >
          {(prop.enum ?? []).map((option) => (
            <option key={option} value={option}>
              {option}
            </option>
          ))}
        </select>
      ) : prop.type === "text" ? (
        <textarea
          id={fieldId}
          name={fieldId}
          rows={4}
          value={typeof value === "string" ? value : ""}
          onChange={(event) => onChange(event.target.value)}
          aria-describedby={describedBy || undefined}
          className={`${shared} resize-y`}
        />
      ) : prop.type === "number" ? (
        <input
          id={fieldId}
          name={fieldId}
          type="number"
          value={typeof value === "number" ? value : 0}
          onChange={(event) => onChange(Number(event.target.value))}
          aria-describedby={describedBy || undefined}
          className={shared}
        />
      ) : prop.type === "boolean" ? (
        <input
          id={fieldId}
          name={fieldId}
          type="checkbox"
          checked={value === true}
          onChange={(event) => onChange(event.target.checked)}
          aria-describedby={describedBy || undefined}
          className="size-3.5 accent-[var(--color-accent)]"
        />
      ) : prop.type === "list" ? (
        <textarea
          id={fieldId}
          name={fieldId}
          rows={4}
          value={Array.isArray(value) ? (value as string[]).join("\n") : ""}
          onChange={(event) =>
            onChange(
              event.target.value
                .split("\n")
                .map((line) => line.trim())
                .filter(Boolean),
            )
          }
          aria-describedby={`${fieldId}-list ${describedBy}`}
          placeholder="One per line"
          className={`${shared} resize-y font-mono text-[11.5px]`}
        />
      ) : (
        <input
          id={fieldId}
          name={fieldId}
          type="text"
          value={typeof value === "string" ? value : ""}
          onChange={(event) => onChange(event.target.value)}
          aria-describedby={`${describedBy} ${fieldId}-list`.trim() || undefined}
          className={shared}
        />
      )}
      {/* The line a plain string field needs too: the list kind says "one per line", the rest
          say what the value is for. Without it the field is a label and a box. */}
      {prop.type === "list" ? (
        <span id={`${fieldId}-list`} className="text-[11px] text-muted">
          One entry per line.
        </span>
      ) : prop.type === "boolean" ? (
        <span className="text-[11px] text-muted">On when checked.</span>
      ) : null}
      {messages}
    </label>
  );
}

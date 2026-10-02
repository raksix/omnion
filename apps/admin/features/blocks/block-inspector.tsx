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
  BlockSettings,
  ContentBlock,
} from "@omnion/types";
import { AlertTriangle, Plus, Search } from "lucide-react";

import { categoryLabel, definitionFor, propValue } from "./block-library";
import { blockSettings } from "./block-tree";

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
    // A structure-only block (`column`) is in the registry because a stored payload carries it,
    // but it is never a choice: there is nowhere outside a Columns block to put one, and an
    // author who could insert it would only discover that at publish time.
    const matches = registry.blocks.filter((entry) => {
      if (entry.structure_only) {
        return false;
      }
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
  /** Writing one of the block's own settings (`meta`). */
  onSetting: (key: keyof BlockSettings, value: string) => void;
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
  onSetting,
  actions,
  breadcrumb,
  onCrumb,
}: InspectorProps) {
  const definition = definitionFor(registry, block.type);
  const settings = blockSettings(block);
  // The API reports a bad setting on a `meta.*` path, and the field that owns it is one of the
  // three below — matching on the key is what puts the message under the right control instead
  // of in the generic issue list, where an author cannot tell which box to fix.
  const metaIssue = issues.find((issue) => issue.path.includes(".meta."));

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
                data-block-crumb
                data-block-crumb-path={crumb.path.join("-")}
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

          {/* Visibility. The control is a single "hidden from" choice rather than two switches,
              because the three states are one setting with three values and the server stores it
              as one. It says plainly that the block is left out of that render entirely — an
              author who thinks this is a CSS toggle would not know the block is still in the
              page for screen readers. */}
          <fieldset className="flex flex-col gap-1.5 border-t border-line pt-3">
            <legend className="text-[11px] tracking-wide text-muted uppercase">Visibility</legend>
            <label htmlFor="block-meta-hide-on" className="flex flex-col gap-1">
              <span className="text-[12px] font-medium">Hide on</span>
              <select
                id="block-meta-hide-on"
                name="block-meta-hide-on"
                data-block-hide-on
                value={settings.hide_on ?? "none"}
                onChange={(event) => onSetting("hide_on", event.target.value)}
                className="w-full rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              >
                <option value="none">Everywhere</option>
                <option value="mobile">Phones (left out of the mobile render)</option>
                <option value="desktop">Desktops (left out of the wide render)</option>
              </select>
              <span className="text-[11px] text-muted">
                A hidden block is not drawn on that screen at all — it is not in the HTML, not in
                the page outline and not for a screen reader.
              </span>
            </label>
            {metaIssue ? (
              <span className="text-[11px] text-accent-strong">{metaIssue.message}</span>
            ) : null}
          </fieldset>

          {/* Advanced: the addressing settings. They are text fields rather than schema props
              because nothing about them depends on the block type — an anchor on a heading and
              an anchor on a pricing table mean exactly the same thing. */}
          <fieldset className="flex flex-col gap-1.5 border-t border-line pt-3">
            <legend className="text-[11px] tracking-wide text-muted uppercase">Advanced</legend>
            <SettingField
              id="block-meta-anchor"
              label="Anchor"
              hint="A page link can point at it as /page#anchor."
              value={settings.anchor ?? ""}
              onChange={(value) => onSetting("anchor", value)}
            />
            <SettingField
              id="block-meta-aria"
              label="Accessible label"
              hint="Names the block for someone who cannot see it."
              value={settings.aria_label ?? ""}
              onChange={(value) => onSetting("aria_label", value)}
            />
            <SettingField
              id="block-meta-class"
              label="CSS class"
              hint="Extra class names the active theme may style."
              value={settings.class ?? ""}
              onChange={(value) => onSetting("class", value)}
            />
          </fieldset>
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

type SettingFieldProps = {
  /** DOM id, also the input's `name`. */
  id: string;
  /** What the field is called. */
  label: string;
  /** What the value is for — a setting is invisible, so its hint is the documentation. */
  hint: string;
  /** The value the block carries. */
  value: string;
  /** Writing a new value; an empty string clears the setting. */
  onChange: (value: string) => void;
};

/**
 * One of the block's own settings.
 *
 * Deliberately a plain labelled input rather than a generated field: a setting has no schema,
 * no default and no validation rule of its own, so the only thing the author needs from it is
 * to know what it does. Clearing the box clears the setting — that is what the empty state
 * means, and it is why `setSetting` deletes the key rather than storing an empty string.
 */
function SettingField({ id, label, hint, value, onChange }: SettingFieldProps) {
  const hintId = `${id}-hint`;
  return (
    <label htmlFor={id} className="flex flex-col gap-1">
      <span className="text-[12px] font-medium">{label}</span>
      <input
        id={id}
        name={id}
        type="text"
        value={value}
        onChange={(event) => onChange(event.target.value)}
        aria-describedby={hintId}
        className="w-full rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
      />
      <span id={hintId} className="text-[11px] text-muted">
        {hint}
      </span>
    </label>
  );
}

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

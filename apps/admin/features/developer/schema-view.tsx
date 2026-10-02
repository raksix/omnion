"use client";

/**
 * `/developer/graphql/schema` — the schema explorer (REQ-130, slice 2).
 *
 * ## The rule this screen renders is "absent, not nulled"
 *
 * The request: *"A type whose read permission the caller lacks is absent from the schema entirely —
 * not nulled at runtime — so introspection cannot leak shape or existence."* Two lists, two
 * audiences, on purpose:
 *
 * * **Visible types** are what a caller can introspect. Their SDL is rendered verbatim from the
 *   API's own `sdl()`, which already omits everything withheld.
 * * **Withheld types** are named, because the person reading this screen is an *administrator*
 *   asking why an integrator says a type does not exist. Answering that with an empty list is the
 *   "documented but unreachable" shape; answering it with the name is the whole point of the
 *   explorer. The screen says so in words rather than leaving the distinction implicit, because a
 *   withheld list that looked like part of the schema would be read as a leak.
 *
 * ## The role diff exists so the effective-schema rule is visible, not documented
 *
 * *"a `Compare with…` action rendering the field-level diff between two roles."* The diff's value
 * is the permission on each line: `Page.revisions` missing from one role is a support ticket
 * unless the screen also says `content.pages.read`, which is what it is missing.
 *
 * Keyboard: `d` compares, `s` shows the SDL, `r` refreshes. Under `sm:` the two columns stack and
 * each field row becomes its own line.
 */

import { useCallback, useEffect, useState } from "react";

import { AlertTriangle, EyeOff, GitCompare, RefreshCw } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchSchema,
  fetchSchemaDiff,
  type SchemaDiffResponse,
  type SchemaResponse,
} from "@/lib/graphql-api";

export function SchemaExplorerView() {
  const [schema, setSchema] = useState<SchemaResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [showSdl, setShowSdl] = useState(false);
  const [roleId, setRoleId] = useState("");
  const [diff, setDiff] = useState<SchemaDiffResponse | null>(null);
  const [diffError, setDiffError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    setError(null);
    try {
      setSchema(await fetchSchema());
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : "The schema could not be read.");
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const compare = useCallback(async () => {
    if (!roleId) {
      setDiffError("Pick a role to compare against first.");
      return;
    }
    setBusy(true);
    setDiffError(null);
    try {
      setDiff(await fetchSchemaDiff(roleId));
    } catch (caught) {
      setDiff(null);
      setDiffError(caught instanceof ApiError ? caught.message : "The comparison could not be made.");
    } finally {
      setBusy(false);
    }
  }, [roleId]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (target?.tagName === "INPUT" || target?.tagName === "SELECT" || target?.tagName === "TEXTAREA") {
        return;
      }
      if (event.key === "d") {
        event.preventDefault();
        void compare();
      } else if (event.key === "s") {
        event.preventDefault();
        setShowSdl((open) => !open);
      } else if (event.key === "r") {
        event.preventDefault();
        void load();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [compare, load]);

  if (loading) return <LoadingTable columns={3} rows={4} />;

  if (error) {
    return (
      <div role="alert" className="flex items-start gap-2 rounded-md border border-red-300 bg-red-50 px-3 py-2.5 text-[13px] text-red-900 dark:border-red-800 dark:bg-red-950/40 dark:text-red-200">
        <AlertTriangle size={16} className="mt-0.5 shrink-0" aria-hidden />
        <span>
          {error}{" "}
          <button type="button" onClick={() => void load()} className="inline-flex underline">
            Try again
          </button>
        </span>
      </div>
    );
  }

  if (!schema) {
    return (
      <EmptyState
        title="The schema is not available"
        hint="The endpoint answered without a schema. Reload, and check that the API is running this build."
      />
    );
  }

  return (
    <div className="flex flex-col gap-5" data-view="graphql-schema">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-[12.5px] text-muted">
          {schema.schema.types.length} visible type{schema.schema.types.length === 1 ? "" : "s"} ·{" "}
          {schema.permissions.length} resolved permission
          {schema.permissions.length === 1 ? "" : "s"} · guarded by{" "}
          <code className="font-mono">{schema.read_permission}</code>
        </p>
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            onClick={() => setShowSdl((open) => !open)}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12px]"
          >
            {showSdl ? "Hide" : "Show"} SDL <kbd className="text-[10.5px]">s</kbd>
          </button>
          <button
            type="button"
            onClick={() => void load()}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12px]"
          >
            <RefreshCw size={13} aria-hidden /> Refresh <kbd className="text-[10.5px]">r</kbd>
          </button>
        </div>
      </div>

      {showSdl ? (
        <section aria-label="SDL for your permissions" className="flex flex-col gap-1.5">
          <p className="text-[12px] text-muted">
            This is the document a caller receives from introspection. Withheld types do not appear
            in it at all — not as <code className="font-mono">null</code>, and not as a comment.
          </p>
          <pre className="max-h-96 overflow-auto rounded-md border border-line bg-quiet-soft p-3 font-mono text-[11.5px] leading-relaxed">
            {schema.sdl || "This permission set exposes no types."}
          </pre>
          <p className="text-[11.5px] text-muted">
            Cache key: <span className="font-mono break-all">{schema.cache_key}</span>
          </p>
        </section>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-2">
        <section aria-labelledby="visible-heading" className="flex flex-col gap-2">
          <h2 id="visible-heading" className="text-[13px] font-medium">
            Visible to you
          </h2>
          {schema.schema.types.length === 0 ? (
            <EmptyState
              title="Your permission set exposes no types"
              hint="Nothing in the catalogue is readable with the permissions you hold. The withheld list below names what you cannot see."
            />
          ) : (
            <ul className="flex flex-col gap-2">
              {schema.schema.types.map((typeDefinition) => (
                <li
                  key={typeDefinition.name}
                  data-graphql-type={typeDefinition.name}
                  className="rounded-md border border-line bg-surface px-3 py-2.5"
                >
                  <p className="font-mono text-[13px] font-medium">{typeDefinition.name}</p>
                  {typeDefinition.fields.length === 0 ? (
                    <p className="mt-1 text-[12px] text-muted">
                      No field on this type is visible to you.
                    </p>
                  ) : (
                    <ul className="mt-1.5 flex flex-col gap-1">
                      {typeDefinition.fields.map((field) => (
                        <li
                          key={field.name}
                          className="flex flex-wrap items-baseline gap-x-2 text-[12.5px]"
                        >
                          <span className="font-mono">{field.name}</span>
                          {field.returns ? (
                            <span className="text-muted">→ {field.returns}</span>
                          ) : null}
                          {field.is_mutation ? (
                            <span className="rounded border border-blue-300 px-1 text-[10.5px] text-blue-700 dark:border-blue-800 dark:text-blue-300">
                              mutation
                            </span>
                          ) : null}
                          {field.requires ? (
                            <span className="text-[11px] text-muted">requires {field.requires}</span>
                          ) : null}
                        </li>
                      ))}
                    </ul>
                  )}
                </li>
              ))}
            </ul>
          )}
        </section>

        <section aria-labelledby="withheld-heading" className="flex flex-col gap-2">
          <h2 id="withheld-heading" className="text-[13px] font-medium">
            Withheld from you
          </h2>
          <p className="flex items-start gap-1.5 text-[12px] text-muted">
            <EyeOff size={13} aria-hidden className="mt-0.5 shrink-0" />
            These types exist on the platform and your permission set cannot read them. A caller
            never learns they exist — introspection omits them — so this list is an administrator's
            view, not part of the schema.
          </p>
          {schema.schema.withheld.length === 0 ? (
            <p className="text-[12.5px] text-muted">Nothing is withheld from your permission set.</p>
          ) : (
            <ul className="flex flex-wrap gap-1.5">
              {schema.schema.withheld.map((name) => (
                <li
                  key={name}
                  data-graphql-withheld={name}
                  className="rounded-md border border-line bg-quiet-soft px-2 py-1 font-mono text-[11.5px] text-muted"
                >
                  {name}
                </li>
              ))}
            </ul>
          )}
        </section>
      </div>

      {/* The role diff. */}
      <section aria-labelledby="diff-heading" className="flex flex-col gap-2">
        <div className="flex flex-wrap items-end justify-between gap-2">
          <h2 id="diff-heading" className="text-[13px] font-medium">
            Compare with a role
          </h2>
          <div className="flex flex-wrap items-end gap-2">
            <label className="flex flex-col gap-1 text-[12px]">
              <span className="text-muted">Role</span>
              <select
                value={roleId}
                onChange={(event) => {
                  setRoleId(event.target.value);
                  setDiff(null);
                }}
                className="rounded-md border border-line bg-background px-2.5 py-1.5 text-[12px]"
              >
                <option value="">Pick a role…</option>
                {schema.roles.map((role) => (
                  <option key={role.id} value={role.id}>
                    {role.name} — {role.permission_count} GraphQL permissions
                  </option>
                ))}
              </select>
            </label>
            <button
              type="button"
              disabled={busy}
              onClick={() => void compare()}
              className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12px] disabled:opacity-60"
            >
              <GitCompare size={13} aria-hidden /> Compare <kbd className="text-[10.5px]">d</kbd>
            </button>
          </div>
        </div>

        {diffError ? (
          <p role="alert" className="text-[12px] text-red-700 dark:text-red-300">
            {diffError}
          </p>
        ) : null}

        {diff ? (
          <div className="flex flex-col gap-2">
            <p className="text-[12.5px]">
              {diff.identical ? (
                <>
                  Your schema and <span className="font-medium">{diff.theirs}</span> resolve to the
                  same GraphQL surface.
                </>
              ) : (
                <>
                  <span className="font-medium">Your schema</span> beside{" "}
                  <span className="font-medium">{diff.theirs}</span>
                </>
              )}
            </p>

            {diff.only_theirs.length > 0 ? (
              <div className="rounded-md border border-line bg-surface p-3">
                <p className="text-[12.5px] font-medium">
                  {diff.theirs} can reach what you cannot
                </p>
                <ul className="mt-1.5 flex flex-col gap-1">
                  {diff.only_theirs.map((line) => (
                    <li key={line.path} className="flex flex-wrap items-baseline gap-x-2 text-[12px]">
                      <span className="font-mono">{line.path}</span>
                      <span className="text-muted">
                        {line.requires ? `needs ${line.requires}` : "no field holds this type's data"}
                      </span>
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}

            {diff.only_mine.length > 0 ? (
              <div className="rounded-md border border-line bg-surface p-3">
                <p className="text-[12.5px] font-medium">You can reach what {diff.theirs} cannot</p>
                <ul className="mt-1.5 flex flex-col gap-1">
                  {diff.only_mine.map((line) => (
                    <li key={line.path} className="flex flex-wrap items-baseline gap-x-2 text-[12px]">
                      <span className="font-mono">{line.path}</span>
                      <span className="text-muted">
                        {line.requires ? `needs ${line.requires}` : "no field holds this type's data"}
                      </span>
                    </li>
                  ))}
                </ul>
              </div>
            ) : null}
          </div>
        ) : null}
      </section>
    </div>
  );
}
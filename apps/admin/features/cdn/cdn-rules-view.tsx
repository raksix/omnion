"use client";

/**
 * `/cdn/rules` — the cache-rule table and the rule form (REQ-011, slice 1).
 *
 * Five things on this screen are not decoration, and each exists because the shortcut is
 * wrong rather than because a designer asked for it:
 *
 * - **The order is the rule.** `priority` is not a number a person tunes; it is the order the
 *   matcher takes, and "the first enabled rule that matches wins" means a rule above a
 *   broad one can make the broad one unreachable forever. The table therefore shows the
 *   order as a rank, and a rule that matches everything is labelled as such rather than
 *   sitting quietly at the bottom where it looks harmless.
 * - **Reorder sends the whole list.** A reorder that renumbers one row is the operation that
 *   leaves two rules claiming the same priority, and the matcher then breaks the tie by row
 *   order — which is not the order the drag showed. The server takes the complete list for
 *   exactly that reason, and the panel sends it even when only one row moved.
 * - **An unreadable rule is a row, not a filter.** A rule whose pattern stopped compiling
 *   cannot match anything, and hiding it would let a site behave as if it did not exist.
 *   It is rendered with the reason, above the table, where a person can act on it.
 * - **The pattern is tested against a real path while it is typed.** A glob that matches
 *   nothing is a valid rule the API will happily store, and the only way to notice is to
 *   look at a page that did not become cacheable. The tester is local — it is the same
 *   `*`/`**` semantics the server uses — and it says "matches"/"does not match" against
 *   whatever sample URL is in the box.
 * - **Deleting asks for the name.** A rule can carry a wildcard that has been quietly
 *   serving an hour-old page for a week; the confirm names the rule so the answer is
 *   deliberate rather than a reflex.
 */
import { useCallback, useEffect, useMemo, useState } from "react";

import {
  ArrowDown,
  ArrowUp,
  Copy,
  GripVertical,
  Plus,
  RefreshCw,
  Trash2,
  TriangleAlert,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { StatusBadge } from "@/components/status-badge";
import {
  ApiError,
  createCdnRule,
  deleteCdnRule,
  fetchCdnRules,
  reorderCdnRules,
  setCdnRuleEnabled,
  updateCdnRule,
} from "@/lib/api";
import { useSites } from "@/lib/sites";
import type { CdnCacheRule } from "@/lib/types";

/** The methods a rule can apply to, and whether a response to one may be stored. */
const METHODS = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"] as const;

/** The largest TTL the API accepts, restated here so the form refuses before it posts. */
const TTL_CAP = 31_536_000;

/**
 * Does a glob match a path?
 *
 * The same semantics the server compiles: `**` crosses `/`, `*` does not, and `**` alone is
 * "everything". Implemented on the client so the tester answers as the operator types
 * rather than after a round trip; the API re-validates the pattern on save, so a mistake
 * here costs nothing but a wrong-looking tick.
 */
function globMatches(pattern: string, path: string): boolean {
  if (pattern === "" || path === "") {
    return false;
  }
  if (pattern === "/**" || pattern === "**") {
    return true;
  }
  const source = pattern
    .split("**")
    .map((part) =>
      part
        .split("*")
        .map((chunk) => chunk.replace(/[.+?^${}()|[\]\\]/g, "\\$&"))
        .join("[^/]*"),
    )
    .join(".*");
  return new RegExp(`^${source}$`).test(path);
}

/** A TTL the way a person reads it: `90s`, `5m`, `1h`, `7d`. */
function readTtl(seconds: number): string {
  if (seconds === 0) {
    return "0";
  }
  if (seconds % 86_400 === 0) {
    return `${seconds / 86_400}d`;
  }
  if (seconds % 3_600 === 0) {
    return `${seconds / 3_600}h`;
  }
  if (seconds % 60 === 0) {
    return `${seconds / 60}m`;
  }
  return `${seconds}s`;
}

/** A form's own state, kept separate from the saved row so typing is not a save. */
type Draft = {
  name: string;
  path_pattern: string;
  sample_path: string;
  methods: string[];
  edge_ttl_seconds: string;
  browser_ttl_seconds: string;
  swr_seconds: string;
  query_include: string;
  language_cookie: boolean;
  bypass_cookies: string;
  bypass_queries: string;
  bypass_headers: string;
  enabled: boolean;
};

const BLANK: Draft = {
  name: "",
  path_pattern: "/**",
  sample_path: "/",
  methods: ["GET", "HEAD"],
  edge_ttl_seconds: "300",
  browser_ttl_seconds: "60",
  swr_seconds: "0",
  query_include: "",
  language_cookie: false,
  bypass_cookies: "",
  bypass_queries: "",
  bypass_headers: "",
  enabled: true,
};

function toDraft(rule: CdnCacheRule): Draft {
  return {
    name: rule.name,
    path_pattern: rule.path_pattern,
    sample_path: "/",
    methods: [...rule.methods],
    edge_ttl_seconds: String(rule.edge_ttl_seconds),
    browser_ttl_seconds: String(rule.browser_ttl_seconds),
    swr_seconds: String(rule.swr_seconds),
    query_include: rule.cache_key.query_include.join(", "),
    language_cookie: rule.cache_key.language_cookie,
    bypass_cookies: rule.bypass.cookie_names.join(", "),
    bypass_queries: rule.bypass.query_params.join(", "),
    bypass_headers: rule.bypass.header_names.join(", "),
    enabled: rule.enabled,
  };
}

const csv = (value: string): string[] =>
  value
    .split(",")
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0);

/** The whole screen: the table, the filters, the form and the reorder. */
export function CdnRulesView() {
  const { selectedSite } = useSites();
  const siteId = selectedSite?.id ?? null;

  const [rules, setRules] = useState<CdnCacheRule[] | null>(null);
  const [unreadable, setUnreadable] = useState<{ id: string; reason: string }[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [onlyEnabled, setOnlyEnabled] = useState<"all" | "enabled" | "disabled">("all");
  const [filter, setFilter] = useState("");
  const [editing, setEditing] = useState<CdnCacheRule | null | "new">(null);
  const [draft, setDraft] = useState<Draft>(BLANK);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [reloadToken, setReloadToken] = useState(0);
  const [confirming, setConfirming] = useState<string | null>(null);

  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    if (!siteId) {
      setRules(null);
      return;
    }
    let cancelled = false;
    setRules(null);
    setError(null);
    setUnreadable([]);
    fetchCdnRules(siteId)
      .then((answer) => {
        if (cancelled) {
          return;
        }
        setRules(answer.rules);
        setUnreadable(answer.unreadable);
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(cause instanceof ApiError ? cause.message : "The rules could not be loaded.");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [siteId, reloadToken]);

  const shown = useMemo(() => {
    if (!rules) {
      return [];
    }
    const needle = filter.trim().toLowerCase();
    return rules.filter((rule) => {
      if (onlyEnabled === "enabled" && !rule.enabled) {
        return false;
      }
      if (onlyEnabled === "disabled" && rule.enabled) {
        return false;
      }
      if (needle && !rule.name.toLowerCase().includes(needle)) {
        return false;
      }
      return true;
    });
  }, [rules, onlyEnabled, filter]);

  // The rank is the *stored* order, not the filtered one: a filter that renumbered the rows
  // would tell the operator that a rule moved when nothing moved.
  const rankOf = (rule: CdnCacheRule): number =>
    (rules ?? []).findIndex((candidate) => candidate.id === rule.id) + 1;

  const openNew = () => {
    setDraft(BLANK);
    setFieldError(null);
    setEditing("new");
  };

  const openEdit = (rule: CdnCacheRule) => {
    setDraft(toDraft(rule));
    setFieldError(null);
    setEditing(rule);
  };

  const closeForm = () => {
    setEditing(null);
    setFieldError(null);
  };

  /** Move one rule up or down, then persist the whole resulting order. */
  const move = async (rule: CdnCacheRule, direction: -1 | 1) => {
    if (!siteId || !rules) {
      return;
    }
    const from = rules.findIndex((candidate) => candidate.id === rule.id);
    const to = from + direction;
    if (to < 0 || to >= rules.length) {
      return;
    }
    const next = [...rules];
    [next[from], next[to]] = [next[to], next[from]];
    setBusy(true);
    setError(null);
    try {
      const answer = await reorderCdnRules(
        siteId,
        next.map((candidate) => candidate.id),
      );
      setRules(answer.rules);
      setUnreadable(answer.unreadable);
      setNotice(`"${rule.name}" is now ${direction === -1 ? "above" : "below"} its neighbour.`);
    } catch (cause: unknown) {
      // The optimistic order is discarded by the reload rather than left on screen: a table
      // showing an order the server refused is a table the operator will build on.
      setError(cause instanceof ApiError ? cause.message : "The new order could not be saved.");
      reload();
    } finally {
      setBusy(false);
    }
  };

  const toggle = async (rule: CdnCacheRule) => {
    if (!siteId) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await setCdnRuleEnabled(rule.id, siteId, !rule.enabled);
      setNotice(`"${rule.name}" is now ${rule.enabled ? "off" : "on"}.`);
      reload();
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The rule could not be changed.");
    } finally {
      setBusy(false);
    }
  };

  const duplicate = async (rule: CdnCacheRule) => {
    if (!siteId) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await createCdnRule({
        site_id: siteId,
        name: `${rule.name} copy`,
        path_pattern: rule.path_pattern,
        methods: rule.methods,
        edge_ttl_seconds: rule.edge_ttl_seconds,
        browser_ttl_seconds: rule.browser_ttl_seconds,
        swr_seconds: rule.swr_seconds,
        cache_key: rule.cache_key,
        bypass: rule.bypass,
        enabled: false,
      });
      setNotice(`"${rule.name} copy" was added, switched off.`);
      reload();
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The rule could not be duplicated.");
    } finally {
      setBusy(false);
    }
  };

  const remove = async (rule: CdnCacheRule) => {
    if (!siteId) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await deleteCdnRule(rule.id, siteId);
      setNotice(`"${rule.name}" was deleted.`);
      setConfirming(null);
      reload();
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The rule could not be deleted.");
    } finally {
      setBusy(false);
    }
  };

  const save = async () => {
    if (!siteId) {
      return;
    }
    const name = draft.name.trim();
    if (name.length === 0) {
      setFieldError({ field: "name", message: "A rule needs a name." });
      return;
    }
    if (draft.path_pattern.trim().length === 0) {
      setFieldError({ field: "path_pattern", message: "A rule needs a path pattern." });
      return;
    }
    const ttl = Number(draft.edge_ttl_seconds);
    if (!Number.isInteger(ttl) || ttl < 0 || ttl > TTL_CAP) {
      setFieldError({
        field: "edge_ttl_seconds",
        message: `The edge TTL must be a whole number of seconds between 0 and ${TTL_CAP.toLocaleString("en")}.`,
      });
      return;
    }
    const browser = Number(draft.browser_ttl_seconds);
    if (!Number.isInteger(browser) || browser < 0 || browser > TTL_CAP) {
      setFieldError({
        field: "browser_ttl_seconds",
        message: `The browser TTL must be a whole number of seconds between 0 and ${TTL_CAP.toLocaleString("en")}.`,
      });
      return;
    }
    if (draft.methods.length === 0) {
      setFieldError({ field: "methods", message: "Pick at least one method." });
      return;
    }

    const input = {
      site_id: siteId,
      name,
      path_pattern: draft.path_pattern.trim(),
      methods: draft.methods,
      edge_ttl_seconds: ttl,
      browser_ttl_seconds: browser,
      swr_seconds: Number(draft.swr_seconds) || 0,
      cache_key: {
        query_include: csv(draft.query_include),
        language_cookie: draft.language_cookie,
      },
      bypass: {
        cookie_names: csv(draft.bypass_cookies),
        query_params: csv(draft.bypass_queries),
        header_names: csv(draft.bypass_headers),
      },
      enabled: draft.enabled,
    };

    setBusy(true);
    setFieldError(null);
    try {
      if (editing === "new") {
        await createCdnRule(input);
        setNotice(`"${name}" was added.`);
      } else if (editing) {
        await updateCdnRule(editing.id, input);
        setNotice(`"${name}" was saved.`);
      }
      setEditing(null);
      reload();
    } catch (cause: unknown) {
      // A refusal that names a field is put under that field; one that does not is shown as
      // a banner, because a message with nowhere to go is a message nobody reads.
      if (cause instanceof ApiError && typeof cause.details?.field === "string") {
        setFieldError({ field: cause.details.field, message: cause.message });
      } else {
        setError(cause instanceof ApiError ? cause.message : "The rule could not be saved.");
      }
    } finally {
      setBusy(false);
    }
  };

  if (!siteId) {
    return (
      <EmptyState
        title="No site selected"
        hint="Cache rules belong to a site. Pick one in the switcher above."
      />
    );
  }

  const sample = draft.sample_path.trim();
  const testVerdict = sample.length > 0 ? globMatches(draft.path_pattern.trim(), sample) : null;

  return (
    <div className="flex flex-col gap-4">
      {error ? (
        <p
          role="alert"
          className="rounded-xl border border-accent/30 bg-accent-soft px-4 py-3 text-[12.5px] text-accent-strong"
        >
          {error}
        </p>
      ) : null}
      {notice ? (
        <p
          role="status"
          className="rounded-xl border border-positive/25 bg-positive-soft px-4 py-3 text-[12.5px] text-positive"
        >
          {notice}
        </p>
      ) : null}

      {unreadable.length > 0 ? (
        <div className="rounded-xl border border-caution/30 bg-caution-soft px-4 py-3">
          <p className="flex items-center gap-2 text-[12.5px] font-medium text-caution">
            <TriangleAlert className="size-3.5" aria-hidden />
            {`${unreadable.length} rule${unreadable.length === 1 ? "" : "s"} cannot be matched`}
          </p>
          <ul className="mt-2 flex flex-col gap-1.5">
            {unreadable.map((broken) => (
              <li key={broken.id} className="text-[12px] text-muted">
                <button
                  type="button"
                  onClick={() => setConfirming(broken.id)}
                  className="font-mono text-[11.5px] underline decoration-dotted underline-offset-2"
                >
                  {broken.id}
                </button>
                {` — ${broken.reason} It matches nothing until it is fixed or deleted.`}
              </li>
            ))}
          </ul>
        </div>
      ) : null}

      {editing ? (
        <section
          aria-label={editing === "new" ? "New cache rule" : `Edit ${editing.name}`}
          className="flex flex-col gap-4 rounded-xl border border-line bg-surface px-4 py-4"
        >
          <div className="flex items-center justify-between gap-3">
            <h2 className="text-[13.5px] font-medium">
              {editing === "new" ? "New rule" : `Editing “${editing.name}”`}
            </h2>
            <button
              type="button"
              onClick={closeForm}
              className="rounded-lg border border-line px-2.5 py-1 text-[12px] text-muted transition hover:text-ink"
            >
              Close
            </button>
          </div>

          <div className="grid gap-4 md:grid-cols-2">
            <label className="flex flex-col gap-1.5 text-[12.5px]">
              <span className="font-medium">Name</span>
              <input
                value={draft.name}
                data-cdn-rule-name
                onChange={(event) => setDraft({ ...draft, name: event.target.value })}
                placeholder="Public pages"
                className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
              {fieldError?.field === "name" ? (
                <span className="text-[11.5px] text-accent-strong">{fieldError.message}</span>
              ) : null}
            </label>

            <label className="flex flex-col gap-1.5 text-[12.5px]">
              <span className="font-medium">Path pattern</span>
              <input
                value={draft.path_pattern}
                data-cdn-rule-pattern
                onChange={(event) => setDraft({ ...draft, path_pattern: event.target.value })}
                placeholder="/blog/**"
                className="rounded-lg border border-line bg-surface px-3 py-2 font-mono text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
              {fieldError?.field === "path_pattern" ? (
                <span className="text-[11.5px] text-accent-strong">{fieldError.message}</span>
              ) : null}
            </label>
          </div>

          {/* The tester. A rule that matches nothing is a valid rule the server will store,
              so the only place the mistake is visible is here. */}
          <div className="flex flex-wrap items-center gap-2 rounded-lg bg-canvas/70 px-3 py-2.5">
            <label className="flex items-center gap-2 text-[12px] text-muted">
              <span className="font-medium">Test against</span>
              <input
                value={draft.sample_path}
                data-cdn-rule-sample
                onChange={(event) => setDraft({ ...draft, sample_path: event.target.value })}
                placeholder="/blog/post-1"
                className="w-48 rounded-lg border border-line bg-surface px-2.5 py-1.5 font-mono text-[12px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <span
              data-cdn-rule-verdict
              className={`rounded-full px-2.5 py-1 text-[11.5px] font-medium ${
                testVerdict === null
                  ? "bg-quiet-soft text-muted"
                  : testVerdict
                    ? "bg-positive-soft text-positive"
                    : "bg-caution-soft text-caution"
              }`}
            >
              {testVerdict === null
                ? "Type a path to test"
                : testVerdict
                  ? "This rule matches it"
                  : "This rule does not match it"}
            </span>
          </div>

          <fieldset className="flex flex-col gap-2">
            <legend className="text-[12.5px] font-medium">Methods</legend>
            <div className="flex flex-wrap gap-3">
              {METHODS.map((method) => (
                <label key={method} className="flex items-center gap-1.5 text-[12.5px]">
                  <input
                    type="checkbox"
                    data-cdn-rule-method={method}
                    checked={draft.methods.includes(method)}
                    onChange={(event) =>
                      setDraft({
                        ...draft,
                        methods: event.target.checked
                          ? [...draft.methods, method]
                          : draft.methods.filter((entry) => entry !== method),
                      })
                    }
                    className="size-3.5 accent-[var(--color-accent)]"
                  />
                  {method}
                </label>
              ))}
            </div>
            {fieldError?.field === "methods" ? (
              <span className="text-[11.5px] text-accent-strong">{fieldError.message}</span>
            ) : null}
          </fieldset>

          <div className="grid gap-4 md:grid-cols-3">
            <label className="flex flex-col gap-1.5 text-[12.5px]">
              <span className="font-medium">Edge TTL (seconds)</span>
              <input
                value={draft.edge_ttl_seconds}
                data-cdn-rule-edge-ttl
                onChange={(event) => setDraft({ ...draft, edge_ttl_seconds: event.target.value })}
                className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
              {fieldError?.field === "edge_ttl_seconds" ? (
                <span className="text-[11.5px] text-accent-strong">{fieldError.message}</span>
              ) : null}
            </label>
            <label className="flex flex-col gap-1.5 text-[12.5px]">
              <span className="font-medium">Browser TTL (seconds)</span>
              <input
                value={draft.browser_ttl_seconds}
                data-cdn-rule-browser-ttl
                onChange={(event) =>
                  setDraft({ ...draft, browser_ttl_seconds: event.target.value })
                }
                className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
              {fieldError?.field === "browser_ttl_seconds" ? (
                <span className="text-[11.5px] text-accent-strong">{fieldError.message}</span>
              ) : null}
            </label>
            <label className="flex flex-col gap-1.5 text-[12.5px]">
              <span className="font-medium">Stale-while-revalidate (seconds)</span>
              <input
                value={draft.swr_seconds}
                onChange={(event) => setDraft({ ...draft, swr_seconds: event.target.value })}
                className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
          </div>

          <div className="grid gap-4 md:grid-cols-2">
            <label className="flex flex-col gap-1.5 text-[12.5px]">
              <span className="font-medium">Cache key keeps these query parameters</span>
              <input
                value={draft.query_include}
                onChange={(event) => setDraft({ ...draft, query_include: event.target.value })}
                placeholder="utm_source, page"
                className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <label className="flex items-center gap-2 self-end pb-2 text-[12.5px]">
              <input
                type="checkbox"
                data-cdn-rule-language-key
                checked={draft.language_cookie}
                onChange={(event) =>
                  setDraft({ ...draft, language_cookie: event.target.checked })
                }
                className="size-3.5 accent-[var(--color-accent)]"
              />
              Vary on the language cookie
            </label>
          </div>

          <fieldset className="grid gap-3 rounded-lg border border-line px-3 py-3 md:grid-cols-3">
            <legend className="px-1 text-[12.5px] font-medium">Bypass when the request carries</legend>
            <label className="flex flex-col gap-1.5 text-[12.5px]">
              <span className="font-medium">A cookie named</span>
              <input
                value={draft.bypass_cookies}
                data-cdn-rule-bypass-cookies
                onChange={(event) => setDraft({ ...draft, bypass_cookies: event.target.value })}
                placeholder="session, preview"
                className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <label className="flex flex-col gap-1.5 text-[12.5px]">
              <span className="font-medium">A query parameter</span>
              <input
                value={draft.bypass_queries}
                data-cdn-rule-bypass-queries
                onChange={(event) => setDraft({ ...draft, bypass_queries: event.target.value })}
                placeholder="preview, token"
                className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
            <label className="flex flex-col gap-1.5 text-[12.5px]">
              <span className="font-medium">A header</span>
              <input
                value={draft.bypass_headers}
                data-cdn-rule-bypass-headers
                onChange={(event) => setDraft({ ...draft, bypass_headers: event.target.value })}
                placeholder="authorization"
                className="rounded-lg border border-line bg-surface px-3 py-2 text-[13px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
              />
            </label>
          </fieldset>

          <label className="flex items-center gap-2 text-[12.5px]">
            <input
              type="checkbox"
              data-cdn-rule-enabled
              checked={draft.enabled}
              onChange={(event) => setDraft({ ...draft, enabled: event.target.checked })}
              className="size-3.5 accent-[var(--color-accent)]"
            />
            The rule is live
          </label>

          <div className="flex items-center gap-2">
            <button
              type="button"
              onClick={() => void save()}
              disabled={busy}
              data-cdn-rule-save
              className="rounded-lg bg-accent px-3.5 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-60"
            >
              {editing === "new" ? "Add rule" : "Save rule"}
            </button>
            <button
              type="button"
              onClick={closeForm}
              className="rounded-lg border border-line px-3 py-2 text-[12.5px] transition hover:bg-canvas"
            >
              Cancel
            </button>
          </div>
        </section>
      ) : null}

      <div className="overflow-hidden rounded-xl border border-line bg-surface">
        <div className="flex flex-wrap items-center justify-between gap-3 border-b border-line px-4 py-3">
          <div className="flex flex-wrap items-center gap-2">
            <h2 className="text-[13.5px] font-medium">Cache rules</h2>
            <span className="text-[12px] text-muted">
              {rules === null ? "Loading…" : `${rules.length} in precedence order`}
            </span>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <label className="sr-only" htmlFor="cdn-rule-filter">
              Filter rules by name
            </label>
            <input
              id="cdn-rule-filter"
              value={filter}
              onChange={(event) => setFilter(event.target.value)}
              placeholder="Filter by name"
              className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
            />
            <select
              value={onlyEnabled}
              data-cdn-rule-state-filter
              onChange={(event) =>
                setOnlyEnabled(event.target.value as "all" | "enabled" | "disabled")
              }
              aria-label="Filter rules by state"
              className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] outline-none transition focus:border-accent focus:ring-2 focus:ring-accent/15"
            >
              <option value="all">All states</option>
              <option value="enabled">Enabled</option>
              <option value="disabled">Disabled</option>
            </select>
            <button
              type="button"
              onClick={reload}
              aria-label="Reload cache rules"
              className="rounded-lg border border-line bg-surface p-2 text-muted transition hover:text-ink"
            >
              <RefreshCw className="size-3.5" aria-hidden />
            </button>
            <button
              type="button"
              onClick={openNew}
              data-cdn-rule-new
              className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
            >
              <Plus className="size-3.5" aria-hidden />
              New rule
            </button>
          </div>
        </div>

        {rules === null ? (
          <LoadingTable columns={7} />
        ) : shown.length === 0 ? (
          <EmptyState
            title={rules.length === 0 ? "No cache rules yet" : "No rule matches this filter"}
            hint={
              rules.length === 0
                ? "Without a rule every public response is served as private, no-store. That is a safe default, not a fast one."
                : "Clear the filter to see the rest of the table."
            }
            action={
              rules.length === 0 ? (
                <button
                  type="button"
                  onClick={openNew}
                  className="rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong"
                >
                  Write the first rule
                </button>
              ) : undefined
            }
          />
        ) : (
          <>
            <div className="hidden overflow-x-auto md:block">
              <table className="w-full border-collapse text-left text-[13px]">
                <thead>
                  <tr className="bg-canvas/60 text-[11px] font-medium tracking-wide text-muted uppercase">
                    <th scope="col" className="px-4 py-2.5">
                      Priority
                    </th>
                    <th scope="col" className="px-4 py-2.5">
                      Name
                    </th>
                    <th scope="col" className="px-4 py-2.5">
                      Path pattern
                    </th>
                    <th scope="col" className="px-4 py-2.5">
                      Methods
                    </th>
                    <th scope="col" className="px-4 py-2.5">
                      Edge TTL
                    </th>
                    <th scope="col" className="px-4 py-2.5">
                      Browser TTL
                    </th>
                    <th scope="col" className="px-4 py-2.5">
                      State
                    </th>
                    <th scope="col" className="px-4 py-2.5">
                      <span className="sr-only">Actions</span>
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {shown.map((rule) => {
                    const rank = rankOf(rule);
                    const isBroken = unreadable.some((entry) => entry.id === rule.id);
                    return (
                      <tr
                        key={rule.id}
                        data-cdn-rule-row={rule.id}
                        className="border-t border-line transition hover:bg-canvas/60"
                      >
                        <td className="px-4 py-3">
                          <span className="flex items-center gap-1.5">
                            <GripVertical className="size-3.5 text-muted/60" aria-hidden />
                            <span className="font-mono text-[12px]">{rank}</span>
                          </span>
                        </td>
                        <td className="px-4 py-3">
                          <span className="font-medium">{rule.name}</span>
                          {rule.path_pattern === "/**" ? (
                            <span
                              data-cdn-rule-broad
                              className="ml-2 rounded-full bg-caution-soft px-2 py-0.5 text-[10.5px] font-medium text-caution"
                            >
                              matches everything
                            </span>
                          ) : null}
                          {isBroken ? (
                            <span className="ml-2 rounded-full bg-caution-soft px-2 py-0.5 text-[10.5px] font-medium text-caution">
                              unreadable
                            </span>
                          ) : null}
                        </td>
                        <td className="px-4 py-3 font-mono text-[12px] text-muted">
                          {rule.path_pattern}
                        </td>
                        <td className="px-4 py-3 text-muted">{rule.methods.join(", ")}</td>
                        <td className="px-4 py-3 text-muted">{readTtl(rule.edge_ttl_seconds)}</td>
                        <td className="px-4 py-3 text-muted">
                          {readTtl(rule.browser_ttl_seconds)}
                        </td>
                        <td className="px-4 py-3">
                          <StatusBadge status={rule.enabled ? "active" : "disabled"} />
                        </td>
                        <td className="px-4 py-3">
                          <span className="flex items-center justify-end gap-1">
                            <button
                              type="button"
                              onClick={() => void move(rule, -1)}
                              disabled={busy || rank === 1}
                              data-cdn-rule-up={rule.id}
                              aria-label={`Move ${rule.name} up`}
                              className="rounded-lg border border-line p-1.5 text-muted transition hover:text-ink disabled:opacity-40"
                            >
                              <ArrowUp className="size-3.5" aria-hidden />
                            </button>
                            <button
                              type="button"
                              onClick={() => void move(rule, 1)}
                              disabled={busy || rank === rules.length}
                              data-cdn-rule-down={rule.id}
                              aria-label={`Move ${rule.name} down`}
                              className="rounded-lg border border-line p-1.5 text-muted transition hover:text-ink disabled:opacity-40"
                            >
                              <ArrowDown className="size-3.5" aria-hidden />
                            </button>
                            <button
                              type="button"
                              onClick={() => openEdit(rule)}
                              data-cdn-rule-edit={rule.id}
                              className="rounded-lg border border-line px-2 py-1.5 text-[11.5px] transition hover:bg-canvas"
                            >
                              Edit
                            </button>
                            <button
                              type="button"
                              onClick={() => void duplicate(rule)}
                              disabled={busy}
                              data-cdn-rule-duplicate={rule.id}
                              aria-label={`Duplicate ${rule.name}`}
                              className="rounded-lg border border-line p-1.5 text-muted transition hover:text-ink disabled:opacity-60"
                            >
                              <Copy className="size-3.5" aria-hidden />
                            </button>
                            <button
                              type="button"
                              onClick={() => void toggle(rule)}
                              disabled={busy}
                              data-cdn-rule-toggle={rule.id}
                              className="rounded-lg border border-line px-2 py-1.5 text-[11.5px] transition hover:bg-canvas disabled:opacity-60"
                            >
                              {rule.enabled ? "Disable" : "Enable"}
                            </button>
                            {confirming === rule.id ? (
                              <span className="flex items-center gap-1.5">
                                <button
                                  type="button"
                                  onClick={() => void remove(rule)}
                                  disabled={busy}
                                  data-cdn-rule-delete-confirm={rule.id}
                                  className="rounded-lg bg-accent px-2 py-1.5 text-[11.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-60"
                                >
                                  Delete
                                </button>
                                <button
                                  type="button"
                                  onClick={() => setConfirming(null)}
                                  className="rounded-lg border border-line px-2 py-1.5 text-[11.5px] text-muted"
                                >
                                  Keep
                                </button>
                              </span>
                            ) : (
                              <button
                                type="button"
                                onClick={() => setConfirming(rule.id)}
                                data-cdn-rule-delete={rule.id}
                                aria-label={`Delete ${rule.name}`}
                                className="rounded-lg border border-line p-1.5 text-muted transition hover:text-accent-strong"
                              >
                                <Trash2 className="size-3.5" aria-hidden />
                              </button>
                            )}
                          </span>
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>

            {/* Below `md` the same rows as cards. Each card carries the *same* data-*
                hooks as the row it replaces: a hook that exists in only one of the two
                renderings silently halves what a depth pass can drive, and the mobile
                measurements are then of a layout no interaction has ever reached. */}
            <ul className="flex flex-col divide-y divide-line md:hidden">
              {shown.map((rule) => {
                const rank = rankOf(rule);
                return (
                  <li key={rule.id} data-cdn-rule-row={rule.id} className="px-4 py-3.5">
                    <div className="flex items-start justify-between gap-3">
                      <div className="flex min-w-0 flex-col gap-1">
                        <span className="flex items-center gap-2">
                          <span className="font-mono text-[11.5px] text-muted">{rank}</span>
                          <span className="truncate font-medium">{rule.name}</span>
                        </span>
                        <span className="truncate font-mono text-[11.5px] text-muted">
                          {rule.path_pattern}
                        </span>
                        <span className="text-[11.5px] text-muted">
                          {`${rule.methods.join(", ")} · edge ${readTtl(rule.edge_ttl_seconds)} · browser ${readTtl(rule.browser_ttl_seconds)}`}
                        </span>
                      </div>
                      <StatusBadge status={rule.enabled ? "active" : "disabled"} />
                    </div>
                    <div className="mt-2.5 flex flex-wrap gap-1.5">
                      <button
                        type="button"
                        onClick={() => void move(rule, -1)}
                        disabled={busy || rank === 1}
                        data-cdn-rule-up={rule.id}
                        className="rounded-lg border border-line px-2.5 py-2 text-[11.5px] disabled:opacity-40"
                      >
                        Up
                      </button>
                      <button
                        type="button"
                        onClick={() => void move(rule, 1)}
                        disabled={busy || rank === rules.length}
                        data-cdn-rule-down={rule.id}
                        className="rounded-lg border border-line px-2.5 py-2 text-[11.5px] disabled:opacity-40"
                      >
                        Down
                      </button>
                      <button
                        type="button"
                        onClick={() => openEdit(rule)}
                        data-cdn-rule-edit={rule.id}
                        className="rounded-lg border border-line px-2.5 py-2 text-[11.5px]"
                      >
                        Edit
                      </button>
                      <button
                        type="button"
                        onClick={() => void toggle(rule)}
                        disabled={busy}
                        data-cdn-rule-toggle={rule.id}
                        className="rounded-lg border border-line px-2.5 py-2 text-[11.5px] disabled:opacity-60"
                      >
                        {rule.enabled ? "Disable" : "Enable"}
                      </button>
                      {confirming === rule.id ? (
                        <>
                          <button
                            type="button"
                            onClick={() => void remove(rule)}
                            disabled={busy}
                            data-cdn-rule-delete-confirm={rule.id}
                            className="rounded-lg bg-accent px-2.5 py-2 text-[11.5px] font-medium text-white disabled:opacity-60"
                          >
                            Delete
                          </button>
                          <button
                            type="button"
                            onClick={() => setConfirming(null)}
                            className="rounded-lg border border-line px-2.5 py-2 text-[11.5px] text-muted"
                          >
                            Keep
                          </button>
                        </>
                      ) : (
                        <button
                          type="button"
                          onClick={() => setConfirming(rule.id)}
                          data-cdn-rule-delete={rule.id}
                          className="rounded-lg border border-line px-2.5 py-2 text-[11.5px] text-accent-strong"
                        >
                          Delete
                        </button>
                      )}
                    </div>
                  </li>
                );
              })}
            </ul>
          </>
        )}
      </div>
    </div>
  );
}

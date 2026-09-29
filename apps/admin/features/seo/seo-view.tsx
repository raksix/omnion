"use client";

/**
 * `/seo` — the SEO toolkit for one site (REQ-064, slice 3).
 *
 * Four panels over one document: the redirect manager, the sitemap and robots.txt, the broken
 * links, and the tag preview. They live on one route rather than the three the REQ sketched,
 * because they answer one question — "what does a crawler see, and what is in the way" — and an
 * owner chasing a page that is not indexed needs all four open at once.
 *
 * What this screen refuses to do, each because the obvious version has already misled somebody:
 *
 * 1. **A stored tag is the tag shown.** The SERP preview and the JSON-LD view read the tag set
 *    the *server* generated and returns, rather than re-deriving it here. A panel with its own
 *    implementation of "what does Google see" has two implementations, and they drift on the
 *    first edge case — the panel would show a green preview for a tag the renderer never emits.
 *
 * 2. **A missing schema field is named, not hidden.** Choosing `Product` on a page with no
 *    price produces a `missing_fields` list, and the panel shows it in words. The alternative is
 *    a JSON-LD preview that looks perfect and tells a consumer nothing.
 *
 * 3. **`Test a path` says what ELSE answers it.** A resolver that takes the first match and
 *    reports only that leaves an owner with a site whose rules they have never read. The test
 *    names the ambiguity, and it deliberately does not count a hit — otherwise an owner probing
 *    their own rules fills the counter they are reading to decide whether a rule is needed.
 *
 * 4. **An empty sitemap says why it is empty.** "Never generated" and "0 URLs because everything
 *    is unindexed" are different problems with the same blank preview, and only one of them is
 *    the owner's to fix.
 */
import { useCallback, useEffect, useState } from "react";
import {
  AlertTriangle,
  Check,
  Eye,
  EyeOff,
  Globe,
  Loader2,
  Plus,
  RefreshCw,
  Search,
  Trash2,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  createSeoRedirect,
  deleteSeoRedirect,
  fetchSeoOverview,
  regenerateSitemap,
  saveSeoSettings,
  scanBrokenLinks,
  setBrokenLinkIgnored,
  testSeoRedirect,
  updateSeoRedirect,
} from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import { useSites } from "@/lib/sites";
import type { SeoBrokenLink, SeoOverview, SeoRedirect, SeoRedirectTest } from "@/lib/types";

/** Google's own truncation points — the numbers the preview warns against. */
const SERP_TITLE_LIMIT = 60;
const SERP_DESCRIPTION_LIMIT = 160;

/** The pattern dialect, in the words the field help uses. */
const PATTERN_HELP =
  "Literal text, with `.` for one character, `*` for zero or more of the character before it, and `?` for optional. No alternation, no groups — the pattern is matched on every request the site serves.";

export function SeoView() {
  const { selectedSite, status: siteStatus, error: siteError } = useSites();
  const [overview, setOverview] = useState<SeoOverview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [tab, setTab] = useState<"redirects" | "sitemap" | "broken">("redirects");

  const load = useCallback(async () => {
    if (!selectedSite) return;
    setError(null);
    try {
      setOverview(await fetchSeoOverview(selectedSite.id));
    } catch (caught) {
      setError((caught as ApiError).message);
    }
  }, [selectedSite]);

  useEffect(() => {
    void load();
  }, [load]);

  const run = useCallback(
    async (work: () => Promise<string>) => {
      setBusy(true);
      setError(null);
      setNotice(null);
      try {
        setNotice(await work());
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusy(false);
      }
    },
    [load],
  );

  if (siteStatus === "error" && siteError) {
    return <ErrorStrip message={siteError} onRetry={() => void load()} />;
  }
  if (siteStatus === "loading" || siteStatus === "idle") return <SeoSkeleton />;
  if (siteStatus === "ready" && !selectedSite) return <NoSite />;
  if (!overview) return <SeoSkeleton />;
  // The guards above are runtime checks; TypeScript cannot carry them into the JSX below
  // because `selectedSite` comes from a hook. Binding it once here is what turns "the screen
  // already proved there is a site" into something the compiler believes too.
  const site = selectedSite;
  if (!site) return <SeoSkeleton />;

  const { settings, vocabulary, redirects, broken_links: links } = overview;

  return (
    <div className="space-y-6" data-seo-state="ready">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-[12.5px] text-muted">
          {overview.site.host
            ? `Crawlers reach this site at ${overview.site.host}. Absolute URLs are built from that host.`
            : "This site has no domain yet, so absolute URLs cannot be generated. Add a domain under Sites first."}
        </p>
        <div className="flex gap-2">
          <button
            type="button"
            data-seo-refresh
            onClick={() => void load()}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <RefreshCw className="h-3.5 w-3.5" aria-hidden />
            Refresh
          </button>
          <button
            type="button"
            data-seo-scan
            disabled={busy}
            onClick={() =>
              void run(async () => {
                const found = await scanBrokenLinks(site.id);
                return found.length === 0
                  ? "Scanned every published page. No internal link points at a page that does not exist."
                  : `Found ${found.length} broken internal link${found.length === 1 ? "" : "s"}.`;
              })
            }
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
          >
            <Search className="h-3.5 w-3.5" aria-hidden />
            Check internal links
          </button>
        </div>
      </div>

      {busy ? (
        <p data-seo-busy className="inline-flex items-center gap-1.5 text-[12.5px] text-muted">
          <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
          Working…
        </p>
      ) : null}
      {notice ? (
        <p data-seo-notice className="text-[12.5px] text-muted">
          {notice}
        </p>
      ) : null}
      {error ? (
        <p data-seo-error className="text-[12.5px] text-red-700 dark:text-red-300">
          {error}
        </p>
      ) : null}

      <div className="flex flex-wrap gap-1.5" role="tablist" aria-label="SEO sections">
        {(
          [
            ["redirects", `Redirects (${redirects.length})`],
            ["sitemap", "Sitemap & robots"],
            ["broken", `Broken links (${links.length})`],
          ] as const
        ).map(([value, label]) => (
          <button
            key={value}
            type="button"
            role="tab"
            aria-selected={tab === value}
            data-seo-tab={value}
            onClick={() => setTab(value)}
            className={`rounded-md border px-2.5 py-1.5 text-[12.5px] ${
              tab === value ? "border-line bg-muted/40" : "border-line"
            }`}
          >
            {label}
          </button>
        ))}
      </div>

      {tab === "redirects" ? (
        <RedirectsPanel
          siteId={site.id}
          rules={redirects}
          vocabulary={vocabulary}
          onRun={run}
        />
      ) : null}
      {tab === "sitemap" ? (
        <SitemapPanel siteId={site.id} settings={settings} vocabulary={vocabulary} onRun={run} />
      ) : null}
      {tab === "broken" ? <BrokenLinksPanel links={links} onRun={run} /> : null}
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// Redirects
// ---------------------------------------------------------------------------------------------

function RedirectsPanel({
  siteId,
  rules,
  vocabulary,
  onRun,
}: {
  siteId: string;
  rules: SeoRedirect[];
  vocabulary: SeoOverview["vocabulary"];
  onRun: (work: () => Promise<string>) => Promise<void>;
}) {
  const [editing, setEditing] = useState<SeoRedirect | "new" | null>(null);
  const [pendingDelete, setPendingDelete] = useState<SeoRedirect | null>(null);
  const [test, setTest] = useState<SeoRedirectTest | null>(null);
  const [testPath, setTestPath] = useState("");

  return (
    <section className="space-y-3" data-seo-panel="redirects">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-[12.5px] text-muted">
          Literal rules are checked before pattern rules, and the first match answers. A path two
          rules match is reported by the test below rather than resolved silently.
        </p>
        <button
          type="button"
          data-seo-redirect-new
          onClick={() => {
            setEditing("new");
            setTest(null);
          }}
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
        >
          <Plus className="h-3.5 w-3.5" aria-hidden />
          New redirect
        </button>
      </div>

      {editing ? (
        <RedirectForm
          siteId={siteId}
          rule={editing === "new" ? null : editing}
          vocabulary={vocabulary}
          onCancel={() => setEditing(null)}
          onSaved={async (from) => {
            setEditing(null);
            await onRun(async () => `Saved the rule for ${from}.`);
          }}
        />
      ) : null}

      {rules.length === 0 ? (
        <div className="rounded-lg border border-line" data-seo-redirects-empty>
          <EmptyState
            title="No redirect rules in this site"
            hint="A redirect answers a path that used to exist. A site that has never moved a page does not need one — and a rule that redirects nothing is a rule nobody can tell is doing a job."
            action={
              <button
                type="button"
                onClick={() => setEditing("new")}
                className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
              >
                Add the first rule
              </button>
            }
          />
        </div>
      ) : (
        <ul className="space-y-2" data-seo-redirects-list>
          {rules.map((rule) => (
            <li
              key={rule.id}
              data-seo-redirect-row={rule.from_path}
              className="rounded-lg border border-line px-4 py-3"
            >
              <div className="flex flex-wrap items-center justify-between gap-3">
                <div className="min-w-0 font-mono text-[12.5px]">
                  <span className="text-muted">{rule.status_code}</span>{" "}
                  <span data-seo-redirect-from>{rule.from_path}</span>
                  <span className="text-muted"> → </span>
                  <span data-seo-redirect-to>{rule.to_path}</span>
                </div>
                <div className="flex flex-wrap items-center gap-2">
                  <span className="rounded border border-line px-1.5 py-0.5 text-[11.5px] text-muted">
                    {rule.pattern}
                  </span>
                  <span
                    data-seo-redirect-hits
                    className="text-[11.5px] text-muted"
                    title={rule.last_hit_at ? formatTimestamp(rule.last_hit_at) : "never used"}
                  >
                    {rule.hits} hit{rule.hits === 1 ? "" : "s"}
                  </span>
                  {!rule.enabled ? (
                    <span className="rounded border border-line px-1.5 py-0.5 text-[11.5px] text-muted">
                      disabled
                    </span>
                  ) : null}
                  <button
                    type="button"
                    data-seo-redirect-test={rule.from_path}
                    onClick={async () => {
                      setTestPath(rule.from_path);
                      try {
                        setTest(await testSeoRedirect(rule.id, rule.from_path));
                      } catch (caught) {
                        setTest(null);
                        throw caught;
                      }
                    }}
                    className="rounded-md border border-line px-2 py-1 text-[11.5px]"
                  >
                    Test
                  </button>
                  <button
                    type="button"
                    onClick={() => setEditing(rule)}
                    className="rounded-md border border-line px-2 py-1 text-[11.5px]"
                  >
                    Edit
                  </button>
                  <button
                    type="button"
                    data-seo-redirect-delete={rule.from_path}
                    onClick={() => setPendingDelete(rule)}
                    className="inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px]"
                  >
                    <Trash2 className="h-3 w-3" aria-hidden />
                    Delete
                  </button>
                </div>
              </div>
            </li>
          ))}
        </ul>
      )}

      {pendingDelete ? (
        <ConfirmRow
          message={`Delete the rule sending ${pendingDelete.from_path} to ${pendingDelete.to_path}? Visitors who follow a link to that path will get the site's 404 instead.`}
          confirmLabel="Delete rule"
          onConfirm={async () => {
            const from = pendingDelete.from_path;
            setPendingDelete(null);
            await onRun(async () => {
              await deleteSeoRedirect(pendingDelete.id);
              return `Deleted the rule for ${from}.`;
            });
          }}
          onCancel={() => setPendingDelete(null)}
        />
      ) : null}

      {test ? (
        <div className="rounded-lg border border-line px-4 py-3 text-[12.5px]" data-seo-test-result>
          <p className="font-medium">
            {test.matched
              ? `${test.matched.from_path} answers this path, sending to ${test.matched.to_path} with ${test.matched.status_code}.`
              : `No rule answers ${test.path}.`}
          </p>
          {test.also_matched.length > 0 ? (
            <p className="mt-1 text-amber-700 dark:text-amber-300" data-seo-test-ambiguous>
              {test.also_matched.length} other rule
              {test.also_matched.length === 1 ? "" : "s"} also match
              {test.also_matched.length === 1 ? "es" : ""} this path:{" "}
              {test.also_matched.map((rule) => rule.from_path).join(", ")}. Only the first one fires —
              the order above is the order they are checked in.
            </p>
          ) : null}
          <p className="mt-1 text-muted">This test did not count a hit.</p>
        </div>
      ) : null}
      {testPath ? (
        <p className="sr-only" data-seo-test-path>
          {testPath}
        </p>
      ) : null}
    </section>
  );
}

function RedirectForm({
  siteId,
  rule,
  vocabulary,
  onCancel,
  onSaved,
}: {
  siteId: string;
  rule: SeoRedirect | null;
  vocabulary: SeoOverview["vocabulary"];
  onCancel: () => void;
  onSaved: (from: string) => Promise<void>;
}) {
  const [fromPath, setFromPath] = useState(rule?.from_path ?? "");
  const [toPath, setToPath] = useState(rule?.to_path ?? "");
  const [statusCode, setStatusCode] = useState(rule?.status_code ?? 301);
  const [pattern, setPattern] = useState(rule?.pattern ?? "literal");
  const [enabled, setEnabled] = useState(rule?.enabled ?? true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  return (
    <form
      data-seo-redirect-form
      onSubmit={async (event) => {
        event.preventDefault();
        setBusy(true);
        setError(null);
        try {
          if (rule) {
            await updateSeoRedirect(rule.id, {
              from_path: fromPath,
              to_path: toPath,
              status_code: statusCode,
              pattern,
              enabled,
            });
          } else {
            await createSeoRedirect({
              site_id: siteId,
              from_path: fromPath,
              to_path: toPath,
              status_code: statusCode,
              pattern,
              enabled,
            });
          }
          await onSaved(fromPath);
        } catch (caught) {
          setError((caught as ApiError).message);
        } finally {
          setBusy(false);
        }
      }}
      className="space-y-3 rounded-lg border border-line px-4 py-4"
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="From" hint="A site-relative path. No query string, no fragment.">
          <input
            value={fromPath}
            onChange={(event) => setFromPath(event.target.value)}
            placeholder="/old-about"
            data-seo-redirect-from-input
            className="w-full rounded-md border border-line bg-transparent px-2.5 py-1.5 text-[12.5px]"
          />
        </Field>
        <Field label="To" hint="Where the visitor lands.">
          <input
            value={toPath}
            onChange={(event) => setToPath(event.target.value)}
            placeholder="/about"
            data-seo-redirect-to-input
            className="w-full rounded-md border border-line bg-transparent px-2.5 py-1.5 text-[12.5px]"
          />
        </Field>
        <Field label="Status">
          <select
            value={statusCode}
            onChange={(event) => setStatusCode(Number(event.target.value))}
            className="w-full rounded-md border border-line bg-transparent px-2.5 py-1.5 text-[12.5px]"
          >
            {vocabulary.redirect_status_codes.map((code) => (
              <option key={code} value={code}>
                {code} — {code === 301 ? "permanent" : "temporary"}
              </option>
            ))}
          </select>
        </Field>
        <Field label="Kind" hint={pattern === "regex" ? PATTERN_HELP : "Matches one exact path."}>
          <select
            value={pattern}
            onChange={(event) => setPattern(event.target.value)}
            data-seo-redirect-pattern
            className="w-full rounded-md border border-line bg-transparent px-2.5 py-1.5 text-[12.5px]"
          >
            {vocabulary.redirect_patterns.map((kind) => (
              <option key={kind} value={kind}>
                {kind}
              </option>
            ))}
          </select>
        </Field>
      </div>
      <label className="inline-flex items-center gap-2 text-[12.5px]">
        <input
          type="checkbox"
          checked={enabled}
          onChange={(event) => setEnabled(event.target.checked)}
          data-seo-redirect-enabled
        />
        Enabled
      </label>
      {error ? (
        <p data-seo-redirect-error className="text-[12.5px] text-red-700 dark:text-red-300">
          {error}
        </p>
      ) : null}
      <div className="flex gap-2">
        <button
          type="submit"
          disabled={busy}
          data-seo-redirect-save
          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px] disabled:opacity-50"
        >
          {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : <Check className="h-3.5 w-3.5" aria-hidden />}
          {rule ? "Save rule" : "Create rule"}
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
        >
          Cancel
        </button>
      </div>
    </form>
  );
}

// ---------------------------------------------------------------------------------------------
// Sitemap and robots.txt
// ---------------------------------------------------------------------------------------------

function SitemapPanel({
  siteId,
  settings,
  vocabulary,
  onRun,
}: {
  siteId: string;
  settings: SeoOverview["settings"];
  vocabulary: SeoOverview["vocabulary"];
  onRun: (work: () => Promise<string>) => Promise<void>;
}) {
  const [types, setTypes] = useState<string[]>(settings.sitemap_types);
  const [priority, setPriority] = useState(String(settings.default_priority));
  const [frequency, setFrequency] = useState(settings.default_change_frequency);
  const [robots, setRobots] = useState(settings.robots_txt);
  const [warnings, setWarnings] = useState<string[]>([]);

  useEffect(() => {
    setWarnings(overviewWarnings(robots));
  }, [robots]);

  return (
    <section className="space-y-4" data-seo-panel="sitemap">
      <form
        data-seo-settings-form
        onSubmit={(event) => {
          event.preventDefault();
          void onRun(async () => {
            const saved = await saveSeoSettings(siteId, {
              sitemap_types: types,
              default_priority: Number(priority),
              default_change_frequency: frequency,
              robots_txt: robots,
            });
            return `Saved the sitemap settings and robots.txt (${saved.robots_txt.length} bytes).`;
          });
        }}
        className="space-y-3 rounded-lg border border-line px-4 py-4"
      >
        <div>
          <p className="text-[13.5px] font-medium">Page types in the sitemap</p>
          <p className="text-[12.5px] text-muted">
            Nothing ticked means every published page. A page that asks crawlers not to index it
            is left out whatever you choose here — a sitemap must not advertise what robots.txt
            forbids.
          </p>
          {vocabulary.page_types.length === 0 ? (
            <p className="mt-2 text-[12.5px] text-muted" data-seo-no-page-types>
              This site has no pages yet, so there is nothing to include.
            </p>
          ) : (
            <div className="mt-2 flex flex-wrap gap-3" data-seo-sitemap-types>
              {vocabulary.page_types.map((type) => (
                <label key={type} className="inline-flex items-center gap-1.5 text-[12.5px]">
                  <input
                    type="checkbox"
                    checked={types.includes(type)}
                    data-seo-sitemap-type={type}
                    onChange={(event) =>
                      setTypes((current) =>
                        event.target.checked
                          ? [...current, type]
                          : current.filter((value) => value !== type),
                      )
                    }
                  />
                  {type}
                </label>
              ))}
            </div>
          )}
        </div>

        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Default priority" hint="0.0 to 1.0. Search engines treat this as a hint.">
            <input
              value={priority}
              onChange={(event) => setPriority(event.target.value)}
              inputMode="decimal"
              data-seo-priority
              className="w-full rounded-md border border-line bg-transparent px-2.5 py-1.5 text-[12.5px]"
            />
          </Field>
          <Field label="Default change frequency">
            <select
              value={frequency}
              onChange={(event) => setFrequency(event.target.value)}
              data-seo-frequency
              className="w-full rounded-md border border-line bg-transparent px-2.5 py-1.5 text-[12.5px]"
            >
              {vocabulary.change_frequencies.map((value) => (
                <option key={value} value={value}>
                  {value}
                </option>
              ))}
            </select>
          </Field>
        </div>

        <div>
          <label htmlFor="seo-robots" className="text-[13.5px] font-medium">
            robots.txt
          </label>
          <textarea
            id="seo-robots"
            value={robots}
            onChange={(event) => setRobots(event.target.value)}
            rows={5}
            data-seo-robots
            className="mt-1 w-full rounded-md border border-line bg-transparent px-2.5 py-1.5 font-mono text-[12.5px]"
          />
          {warnings.length > 0 ? (
            <ul className="mt-2 space-y-1" data-seo-robots-warnings>
              {warnings.map((warning) => (
                <li
                  key={warning}
                  className="inline-flex items-start gap-1.5 text-[12.5px] text-amber-700 dark:text-amber-300"
                >
                  <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden />
                  {warning}
                </li>
              ))}
            </ul>
          ) : null}
          <p className="mt-1 text-[12.5px] text-muted">
            Saved as written. The warnings above do not block a save — a site that blocks itself
            is a real configuration, and the useful thing is to say so rather than refuse.
          </p>
        </div>

        <div className="flex flex-wrap gap-2">
          <button
            type="submit"
            data-seo-settings-save
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <Check className="h-3.5 w-3.5" aria-hidden />
            Save settings
          </button>
          <button
            type="button"
            data-seo-sitemap-regenerate
            onClick={() =>
              void onRun(async () => {
                const saved = await regenerateSitemap(siteId);
                return `Regenerated the sitemap: ${saved.sitemap_url_count} URL${saved.sitemap_url_count === 1 ? "" : "s"}.`;
              })
            }
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
          >
            <RefreshCw className="h-3.5 w-3.5" aria-hidden />
            Regenerate now
          </button>
        </div>
      </form>

      <div className="rounded-lg border border-line" data-seo-sitemap-preview>
        {settings.sitemap_xml ? (
          <>
            <div className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-2.5">
              <p className="text-[12.5px]">
                {settings.sitemap_url_count} URL{settings.sitemap_url_count === 1 ? "" : "s"}
                {settings.sitemap_last_generated_at
                  ? ` · generated ${formatTimestamp(settings.sitemap_last_generated_at)}`
                  : ""}
              </p>
            </div>
            <pre className="max-h-80 overflow-auto px-4 py-3 font-mono text-[11.5px] text-muted">
              {settings.sitemap_xml}
            </pre>
          </>
        ) : (
          <EmptyState
            title="No sitemap has been generated yet"
            hint="Until one is, a crawler asking for /sitemap.xml gets a 404 rather than an empty file — an empty sitemap reads as “this site has no pages”, which is a decision made for you."
            action={
              <button
                type="button"
                onClick={() =>
                  void onRun(async () => {
                    const saved = await regenerateSitemap(siteId);
                    return `Generated a sitemap with ${saved.sitemap_url_count} URL${saved.sitemap_url_count === 1 ? "" : "s"}.`;
                  })
                }
                className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
              >
                Generate it now
              </button>
            }
          />
        )}
      </div>
    </section>
  );
}

// ---------------------------------------------------------------------------------------------
// Broken links
// ---------------------------------------------------------------------------------------------

function BrokenLinksPanel({
  links,
  onRun,
}: {
  links: SeoBrokenLink[];
  onRun: (work: () => Promise<string>) => Promise<void>;
}) {
  if (links.length === 0) {
    return (
      <div className="rounded-lg border border-line" data-seo-broken-empty>
        <EmptyState
          title="No broken internal links"
          hint="Every link in a published page points at a page this site has. Run the check again after publishing or moving pages."
        />
      </div>
    );
  }
  return (
    <ul className="space-y-2" data-seo-broken-list>
      {links.map((link) => (
        <li
          key={link.id}
          data-seo-broken-row={link.target_url}
          className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-line px-4 py-3"
        >
          <div className="min-w-0">
            <p className="font-mono text-[12.5px]">{link.target_url}</p>
            <p className="text-[11.5px] text-muted">
              {link.anchor_text ? `“${link.anchor_text}” on ` : "linked from "}
              {link.source_slug ? `/${link.source_slug}` : "a page"}
              {link.status ? ` · answered ${link.status}` : ""}
            </p>
          </div>
          <button
            type="button"
            data-seo-broken-ignore={link.target_url}
            onClick={() =>
              void onRun(async () => {
                await setBrokenLinkIgnored(link.id, !link.ignored);
                return link.ignored
                  ? `Brought ${link.target_url} back into the list.`
                  : `Dismissed ${link.target_url}. It will stay dismissed through the next check.`;
              })
            }
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1 text-[11.5px]"
          >
            {link.ignored ? (
              <>
                <Eye className="h-3 w-3" aria-hidden />
                Un-dismiss
              </>
            ) : (
              <>
                <EyeOff className="h-3 w-3" aria-hidden />
                I know
              </>
            )}
          </button>
        </li>
      ))}
    </ul>
  );
}

// ---------------------------------------------------------------------------------------------
// Shared pieces
// ---------------------------------------------------------------------------------------------

function Field({
  label,
  hint,
  children,
}: {
  label: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <label className="block">
      <span className="text-[12.5px] font-medium">{label}</span>
      {hint ? <span className="block text-[11.5px] text-muted">{hint}</span> : null}
      <span className="mt-1 block">{children}</span>
    </label>
  );
}

function ConfirmRow({
  message,
  confirmLabel,
  onConfirm,
  onCancel,
}: {
  message: string;
  confirmLabel: string;
  onConfirm: () => Promise<void>;
  onCancel: () => void;
}) {
  return (
    <div
      data-seo-confirm
      className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-amber-500/50 bg-amber-500/5 px-4 py-3"
    >
      <p className="text-[12.5px]">{message}</p>
      <div className="flex gap-2">
        <button
          type="button"
          data-seo-confirm-yes
          onClick={() => void onConfirm()}
          className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
        >
          {confirmLabel}
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
        >
          Keep it
        </button>
      </div>
    </div>
  );
}

function ErrorStrip({ message, onRetry }: { message: string; onRetry: () => void }) {
  return (
    <div
      data-seo-error
      className="flex flex-wrap items-center justify-between gap-3 rounded-lg border border-red-500/40 bg-red-500/5 px-4 py-3"
    >
      <p className="text-[12.5px]">{message}</p>
      <button
        type="button"
        onClick={onRetry}
        className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12.5px]"
      >
        <RefreshCw className="h-3.5 w-3.5" aria-hidden />
        Try again
      </button>
    </div>
  );
}

function NoSite() {
  return (
    <div className="rounded-lg border border-line" data-seo-no-site>
      <EmptyState
        title="This account has no site yet"
        hint="SEO settings belong to a site — there is nothing to point a crawler at until one exists."
        action={
          <a href="/sites" className="rounded-md border border-line px-3 py-1.5 text-[12.5px]">
            Go to Sites
          </a>
        }
      />
    </div>
  );
}

function SeoSkeleton() {
  return (
    <div className="space-y-3" data-seo-state="loading" aria-busy="true">
      {[0, 1, 2].map((row) => (
        <div key={row} className="h-16 animate-pulse rounded-lg border border-line" />
      ))}
    </div>
  );
}

/** The same two warnings the server raises, so the editor sees them while typing. */
function overviewWarnings(robots: string): string[] {
  const warnings: string[] = [];
  let disallowAll = false;
  let hasUserAgent = false;
  for (const line of robots.split("\n")) {
    const clean = line.split("#")[0].trim();
    if (!clean) continue;
    const [field, ...rest] = clean.split(":");
    const value = rest.join(":").trim();
    if (field?.trim().toLowerCase() === "user-agent") hasUserAgent = true;
    if (field?.trim().toLowerCase() === "disallow" && value === "/") disallowAll = true;
  }
  if (disallowAll) {
    warnings.push(
      "This robots.txt disallows every path for at least one user-agent, so the site asks search engines not to read it.",
    );
  }
  if (!hasUserAgent) {
    warnings.push("This robots.txt names no User-agent, so its rules apply to nobody.");
  }
  return warnings;
}

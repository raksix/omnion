"use client";

/**
 * `/deployment/artifacts` and `/deployment/artifacts/{version}` — what a release publishes
 * (docs/requests/REQ-128, slice 4's screens).
 *
 * ## The screen's job is to make a digest comparable, not decorative
 *
 * The request asks for a copy button that puts "the exact pull reference or checksum" on the
 * clipboard, and the reason is stated in its own words: one-click comparison **on the target host**.
 * So every copy button copies a *complete* reference — `name@sha256:…` for an image, the bare
 * checksum for a file — never a truncated display string. A digest column that wraps mid-string or
 * a copy that hands over `ghcr.io/raksix/omnion/api:0.5.0` where the row showed a digest is the
 * exact failure the request is written against: an operator who pinned a tag because copying the
 * digest was hard has silently lost the property the digest gave them.
 *
 * ## "Not published for this version" is a row, not a blank
 *
 * The API returns `missing_kinds` per release and the full `artifact_kinds` vocabulary, computed
 * server-side. The screen renders those as explicit rows rather than leaving gaps, because a gap is
 * indistinguishable from "the cache has not been filled yet" — and the request distinguishes them
 * on purpose.
 *
 * ## The cached timestamp is on the screen, not implied
 *
 * The whole surface degrades to the cache when the release feed is unreachable, so every release
 * carries `fetched_at` and the screen shows it. An operator comparing digests against a registry
 * needs to know whether they are reading this minute's truth or an hour-old copy.
 *
 * Keyboard: `/` filters, `c` copies the focused row's reference, `r` refreshes, `Esc` clears.
 * Under `sm:` the table becomes cards.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  ArrowLeft,
  Check,
  Copy,
  FileWarning,
  Package,
  RefreshCw,
  Search,
  ShieldAlert,
} from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchArtifacts,
  fetchRelease,
  type ArtifactsResponse,
  type ReleaseArtifact,
  type ReleaseDetail,
} from "@/lib/deployment-api";
import { formatTimestamp } from "@/lib/format";

/** Human bytes. `null` renders as the word rather than a `0`, because a missing size is not zero. */
function humanSize(bytes: number | null): string {
  if (bytes === null || bytes === undefined) return "not stated";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(value >= 10 ? 0 : 1)} ${units[unit]}`;
}

/**
 * The exact reference to copy for an artifact.
 *
 * An image gets `name@digest` when it has one — the digest, never a tag beside it, because a tag
 * next to a digest is the mutable thing the operator was trying to escape — and falls back to the
 * name when the publisher sent no digest at all. A file gets its checksum, which is the only thing
 * a download can be verified against.
 */
function copyReference(artifact: ReleaseArtifact): string {
  if (artifact.kind === "image") {
    return artifact.digest ? `${artifact.name}@${artifact.digest}` : artifact.name;
  }
  return artifact.digest || artifact.name;
}

/** Shorten a digest for the column, never for the clipboard. */
function shortDigest(value: string | null): string {
  if (!value) return "—";
  if (value.startsWith("sha256:")) return `${value.slice(0, 7 + 12)}…`;
  return value.length > 24 ? `${value.slice(0, 16)}…${value.slice(-6)}` : value;
}

/** One clipboard write, with the confirmation the walk can read back. */
function useCopy() {
  const [copied, setCopied] = useState<string | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const timer = useRef<number | null>(null);

  const copy = useCallback(async (key: string, value: string) => {
    const done = (ok: boolean) => {
      if (timer.current !== null) window.clearTimeout(timer.current);
      setCopied(ok ? key : null);
      setFailure(ok ? null : "the clipboard was refused by this browser");
      timer.current = window.setTimeout(() => {
        setCopied(null);
        setFailure(null);
      }, 3000);
    };
    try {
      await navigator.clipboard.writeText(value);
      done(true);
    } catch {
      // `clipboard.writeText` needs a secure context and permission; the walk runs the panel
      // over plain http on loopback, so this is the expected path in QA rather than an exception.
      done(false);
    }
  }, []);

  useEffect(() => () => {
    if (timer.current !== null) window.clearTimeout(timer.current);
  }, []);

  return { copied, failure, copy };
}

// -------------------------------------------------------------------------------------------

function ArtifactTable({
  artifacts,
  emptyHint,
}: {
  artifacts: ReleaseArtifact[];
  emptyHint: string;
}) {
  const { copied, failure, copy } = useCopy();

  if (artifacts.length === 0) {
    return (
      <EmptyState
        title="This release published no artifacts"
        hint={emptyHint}
      />
    );
  }

  return (
    <>
      {failure ? (
        <p role="alert" data-copy-failure className="px-4 py-2 text-[12px] text-danger">
          {failure}
        </p>
      ) : null}
      <table
        className="hidden w-full text-left text-[12.5px] md:table"
        data-artifact-table
      >
        <thead className="text-[11.5px] uppercase tracking-wide text-muted">
          <tr>
            <th scope="col" className="px-4 py-2 font-medium">Artifact</th>
            <th scope="col" className="px-2 py-2 font-medium">Kind</th>
            <th scope="col" className="px-2 py-2 font-medium">Digest / checksum</th>
            <th scope="col" className="px-2 py-2 font-medium">Platforms</th>
            <th scope="col" className="px-2 py-2 font-medium">Size</th>
            <th scope="col" className="px-2 py-2 font-medium">Published</th>
            <th scope="col" className="px-4 py-2 font-medium">Copy</th>
          </tr>
        </thead>
        <tbody>
          {artifacts.map((artifact) => {
            const key = `${artifact.version}:${artifact.kind}:${artifact.name}`;
            const reference = copyReference(artifact);
            return (
              <tr
                key={`${artifact.id}-${artifact.name}`}
                className="border-t border-line"
                data-artifact-row={artifact.name}
                data-artifact-kind={artifact.kind}
                tabIndex={0}
              >
                <td className="px-4 py-2">
                  <p className="break-all font-mono text-[11.5px]">{artifact.name}</p>
                  {!artifact.digest ? (
                    <p className="text-[11px] text-caution">
                      no digest — this reference cannot be pinned
                    </p>
                  ) : null}
                </td>
                <td className="px-2 py-2 font-mono text-[11.5px]">{artifact.kind}</td>
                <td className="px-2 py-2">
                  {/* One unbroken token: a digest that wraps mid-string cannot be compared by eye,
                      which is the one thing this column exists for. */}
                  <code
                    className="block whitespace-nowrap font-mono text-[11px]"
                    title={artifact.digest ?? "none published"}
                    data-artifact-digest
                  >
                    {shortDigest(artifact.digest)}
                  </code>
                </td>
                <td className="px-2 py-2 font-mono text-[11px] text-muted">
                  {artifact.platforms.length > 0 ? artifact.platforms.join(", ") : "—"}
                </td>
                <td className="px-2 py-2 whitespace-nowrap text-[11.5px]">
                  {humanSize(artifact.size_bytes)}
                </td>
                <td className="whitespace-nowrap px-2 py-2 text-muted">
                  {artifact.published_at ? formatTimestamp(artifact.published_at) : "not stated"}
                </td>
                <td className="px-4 py-2">
                  <button
                    type="button"
                    onClick={() => void copy(key, reference)}
                    data-artifact-copy={artifact.name}
                    className="flex items-center gap-1 rounded-md border border-line px-1.5 py-1 text-[11.5px] hover:bg-quiet-soft"
                  >
                    {copied === key ? (
                      <Check className="size-3 text-positive" aria-hidden="true" />
                    ) : (
                      <Copy className="size-3" aria-hidden="true" />
                    )}
                    {copied === key ? "Copied" : "Copy"}
                  </button>
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>

      <ul className="space-y-2 p-3 md:hidden" data-artifact-cards>
        {artifacts.map((artifact) => {
          const key = `${artifact.version}:${artifact.kind}:${artifact.name}`;
          return (
            <li
              key={`${artifact.id}-${artifact.name}`}
              className="rounded-lg border border-line p-3"
              data-artifact-row={artifact.name}
            >
              <div className="flex items-start gap-2">
                <p className="min-w-0 flex-1 break-all font-mono text-[11.5px]">
                  {artifact.name}
                </p>
                <button
                  type="button"
                  onClick={() => void copy(key, copyReference(artifact))}
                  data-artifact-copy={artifact.name}
                  className="flex shrink-0 items-center gap-1 rounded-md border border-line px-1.5 py-1 text-[11.5px]"
                >
                  {copied === key ? (
                    <Check className="size-3 text-positive" aria-hidden="true" />
                  ) : (
                    <Copy className="size-3" aria-hidden="true" />
                  )}
                  {copied === key ? "Copied" : "Copy"}
                </button>
              </div>
              <dl className="mt-2 grid grid-cols-2 gap-x-3 gap-y-1 text-[11.5px]">
                <dt className="text-muted">Kind</dt>
                <dd className="font-mono">{artifact.kind}</dd>
                <dt className="text-muted">Digest</dt>
                <dd className="truncate font-mono" title={artifact.digest ?? undefined}>
                  {shortDigest(artifact.digest)}
                </dd>
                <dt className="text-muted">Size</dt>
                <dd>{humanSize(artifact.size_bytes)}</dd>
                <dt className="text-muted">Published</dt>
                <dd className="truncate">
                  {artifact.published_at ? formatTimestamp(artifact.published_at) : "not stated"}
                </dd>
              </dl>
            </li>
          );
        })}
      </ul>
    </>
  );
}

// -------------------------------------------------------------------------------------------
// The list
// -------------------------------------------------------------------------------------------

export function ArtifactsView() {
  const [data, setData] = useState<ArtifactsResponse | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [version, setVersion] = useState<string | null>(null);
  const filterRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setData(await fetchArtifacts());
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The artifact list could not be read.",
      );
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.tagName === "SELECT";
      if (typing) return;
      if (event.key === "/") {
        event.preventDefault();
        filterRef.current?.focus();
      } else if (event.key === "r") {
        event.preventDefault();
        void load();
      } else if (event.key === "Escape") {
        setFilter("");
        setVersion(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [load]);

  const releases = useMemo(() => {
    const all = data?.releases ?? [];
    const needle = filter.trim().toLowerCase();
    if (!needle) return all;
    return all.filter(
      (release) =>
        release.version.toLowerCase().includes(needle) ||
        release.channel.toLowerCase().includes(needle) ||
        (release.source_commit ?? "").toLowerCase().includes(needle),
    );
  }, [data, filter]);

  const artifacts = useMemo(() => {
    const all = data?.artifacts ?? [];
    if (version) return all.filter((artifact) => artifact.version === version);
    const needle = filter.trim().toLowerCase();
    if (!needle) return all;
    return all.filter(
      (artifact) =>
        artifact.name.toLowerCase().includes(needle) ||
        artifact.kind.toLowerCase().includes(needle) ||
        (artifact.digest ?? "").toLowerCase().includes(needle),
    );
  }, [data, filter, version]);

  return (
    <div className="space-y-4" data-view="deployment-artifacts">
      <section className="rounded-xl border border-line">
        <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <h2 className="flex items-center gap-2 text-[13.5px] font-medium">
            <Package className="size-4 text-accent" aria-hidden="true" />
            Cached releases
          </h2>
          {data ? (
            <span className="text-[12px] text-muted">
              {releases.length} of {data.releases.length}
            </span>
          ) : null}
          <div className="ml-auto flex items-center gap-2">
            <div className="relative">
              <Search
                className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted"
                aria-hidden="true"
              />
              <input
                ref={filterRef}
                value={filter}
                onChange={(event) => setFilter(event.target.value)}
                data-artifact-filter
                placeholder="Filter by version, channel, artifact or digest"
                aria-label="Filter releases and artifacts"
                className="h-8 w-72 rounded-lg border border-line bg-surface pl-7 pr-2 text-[12px] outline-none focus:border-accent"
              />
            </div>
            <button
              type="button"
              onClick={() => void load()}
              data-artifact-refresh
              className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
            >
              <RefreshCw className="h-3 w-3" aria-hidden="true" />
              Refresh
            </button>
          </div>
        </header>

        {error ? (
          <p role="alert" data-artifact-error className="px-4 py-3 text-[12.5px] text-danger">
            {error}
          </p>
        ) : loading && !data ? (
          <div className="px-4 py-3">
            <LoadingTable rows={3} columns={5} />
          </div>
        ) : releases.length === 0 ? (
          <div className="px-4 py-3">
            <EmptyState
              title={
                filter
                  ? "No cached release matches"
                  : "No release manifest is cached yet"
              }
              hint={
                filter
                  ? "Clear the filter to see every cached release."
                  : "The deployment centre reads manifests from its own cache, and a cache with nothing in it has no digest to show. Run an update check from the deployment centre to fetch the release feed."
              }
            />
          </div>
        ) : (
          <ul className="divide-y divide-line" data-release-list>
            {releases.map((release) => {
              const active = version === release.version;
              return (
                <li key={release.version} data-release-row={release.version}>
                  <div className="flex flex-wrap items-start gap-3 px-4 py-3">
                    <div className="min-w-0 flex-1">
                      <div className="flex flex-wrap items-center gap-2">
                        <Link
                          href={`/deployment/artifacts/${encodeURIComponent(release.version)}`}
                          data-release-open={release.version}
                          className="font-mono text-[13px] underline-offset-2 hover:underline"
                        >
                          {release.version}
                        </Link>
                        <span className="rounded-full bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted">
                          {release.channel}
                        </span>
                        {release.core_min ? (
                          // The list has no way to answer "is my build new enough" per release —
                          // that comparison is the detail screen's, against THIS build — so the row
                          // states the requirement rather than guessing at it. A badge here that
                          // compared `core_min` against anything else would be decoration.
                          <span
                            className="rounded-full bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted"
                            data-release-core-min={release.core_min}
                          >
                            needs core {release.core_min}+
                          </span>
                        ) : null}
                      </div>
                      <p className="mt-0.5 text-[11.5px] text-muted">
                        {release.source_commit
                          ? `built from ${release.source_commit}`
                          : "no source commit in the manifest"}
                        {" · "}
                        {release.migration_count} migration
                        {release.migration_count === 1 ? "" : "s"}
                        {" · "}
                        <span data-release-fetched-at={release.version}>
                          cache read {formatTimestamp(release.fetched_at)}
                        </span>
                      </p>
                      {release.missing_kinds.length > 0 ? (
                        <p
                          className="mt-1 flex items-center gap-1 text-[11.5px] text-caution"
                          data-release-missing={release.version}
                        >
                          <FileWarning className="size-3.5 shrink-0" aria-hidden="true" />
                          not published for this version: {release.missing_kinds.join(", ")}
                        </p>
                      ) : null}
                    </div>
                    <button
                      type="button"
                      onClick={() => setVersion(active ? null : release.version)}
                      data-release-toggle={release.version}
                      className="rounded-md border border-line px-2 py-1 text-[11.5px] hover:bg-quiet-soft"
                    >
                      {active ? "Show all releases" : `Artifacts for ${release.version}`}
                    </button>
                  </div>
                </li>
              );
            })}
          </ul>
        )}
      </section>

      <section className="rounded-xl border border-line">
        <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <h2 className="text-[13.5px] font-medium">
            Artifacts {version ? `for ${version}` : "across every cached release"}
          </h2>
          {data ? (
            <span className="text-[12px] text-muted">
              {artifacts.length} of {data.total}
            </span>
          ) : null}
        </header>
        <ArtifactTable
          artifacts={artifacts}
          emptyHint={
            version
              ? `The cache holds no artifact rows for ${version}. Run an update check to fetch its manifest.`
              : "No artifact row has been cached yet. A manifest with no artifact rows is a fetch that has not completed."
          }
        />
      </section>

      <p className="text-[11.5px] text-muted">
        Press <kbd className="rounded border border-line px-1">/</kbd> to filter,{" "}
        <kbd className="rounded border border-line px-1">c</kbd> on a row to copy its reference,{" "}
        <kbd className="rounded border border-line px-1">r</kbd> to refresh. A copy always carries
        the digest: <code className="font-mono">name@sha256:…</code> pins an exact artifact, and a
        tag does not.
      </p>
    </div>
  );
}

// -------------------------------------------------------------------------------------------
// One release
// -------------------------------------------------------------------------------------------

export function ReleaseDetailView({ version }: { version: string }) {
  const [detail, setDetail] = useState<{
    release: ReleaseDetail;
    artifacts: ReleaseArtifact[];
    published_kinds: string[];
    missing_kinds: string[];
  } | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<{ code: string; message: string } | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setDetail(await fetchRelease(version));
    } catch (caught) {
      setError({
        code: caught instanceof ApiError ? caught.code : "unknown_error",
        message:
          caught instanceof ApiError ? caught.message : "The release could not be read.",
      });
    } finally {
      setLoading(false);
    }
  }, [version]);

  useEffect(() => {
    void load();
  }, [load]);

  return (
    <div className="space-y-4" data-view="deployment-release-detail">
      <Link
        href="/deployment/artifacts"
        data-release-back
        className="inline-flex items-center gap-1 text-[12.5px] text-muted hover:text-ink"
      >
        <ArrowLeft className="size-3.5" aria-hidden="true" />
        All cached releases
      </Link>

      {error ? (
        <section className="rounded-xl border border-line p-4">
          <h2 className="flex items-center gap-2 text-[13.5px] font-medium">
            <ShieldAlert className="size-4 text-caution" aria-hidden="true" />
            {error.code === "release_not_cached"
              ? "That version is not in the cache"
              : "The release could not be read"}
          </h2>
          <p role="alert" data-release-error className="mt-1.5 text-[12.5px] text-muted">
            {error.message}
          </p>
          <button
            type="button"
            onClick={() => void load()}
            data-release-retry
            className="mt-3 flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
          >
            <RefreshCw className="h-3 w-3" aria-hidden="true" />
            Try again
          </button>
        </section>
      ) : loading && !detail ? (
        <div className="rounded-xl border border-line px-4 py-3">
          <LoadingTable rows={4} columns={5} />
        </div>
      ) : detail ? (
        <>
          <section className="rounded-xl border border-line p-4">
            <div className="flex flex-wrap items-center gap-2">
              <h2 className="font-mono text-[15px]">{detail.release.version}</h2>
              <span className="rounded-full bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted">
                {detail.release.channel}
              </span>
              <span
                className={`rounded-full px-1.5 py-0.5 text-[11px] ${
                  detail.release.core_minimum_satisfied
                    ? "bg-positive-soft text-positive"
                    : "bg-caution-soft text-caution"
                }`}
                data-core-minimum-satisfied={String(detail.release.core_minimum_satisfied)}
              >
                {detail.release.core_min
                  ? detail.release.core_minimum_satisfied
                    ? `core ${detail.release.core_min}+ satisfied by this build`
                    : `needs core ${detail.release.core_min}+ — this build is older`
                  : "no core minimum declared"}
              </span>
            </div>
            <dl className="mt-3 grid gap-x-6 gap-y-1.5 text-[12.5px] sm:grid-cols-2">
              <div className="flex gap-2">
                <dt className="w-32 shrink-0 text-muted">Source commit</dt>
                <dd className="font-mono" data-release-commit>
                  {detail.release.source_commit ?? "not stated"}
                </dd>
              </div>
              <div className="flex gap-2">
                <dt className="w-32 shrink-0 text-muted">Cache read</dt>
                <dd data-release-fetched>
                  {formatTimestamp(detail.release.fetched_at)}
                </dd>
              </div>
              <div className="flex gap-2">
                <dt className="w-32 shrink-0 text-muted">Migrations</dt>
                <dd data-release-migration-count>{detail.release.migrations.length}</dd>
              </div>
              <div className="flex gap-2">
                <dt className="w-32 shrink-0 text-muted">Publisher&rsquo;s claim</dt>
                <dd data-release-destructive-claim>
                  {detail.release.migrations_destructive
                    ? "declares a destructive migration"
                    : "declares no destructive migration — this is the publisher's claim, not a verification"}
                </dd>
              </div>
              <div className="flex gap-2">
                <dt className="w-32 shrink-0 text-muted">Upgrade notes</dt>
                <dd>
                  {detail.release.upgrade_notes_url ? (
                    <a
                      href={detail.release.upgrade_notes_url}
                      target="_blank"
                      rel="noreferrer noopener"
                      data-release-notes-link
                      className="underline underline-offset-2"
                    >
                      Open the long-form notes
                    </a>
                  ) : (
                    "no long-form notes URL in this manifest"
                  )}
                </dd>
              </div>
            </dl>

            {detail.release.migrations.length > 0 ? (
              <details className="mt-3">
                <summary className="cursor-pointer text-[12.5px] text-muted">
                  The {detail.release.migrations.length} migration
                  {detail.release.migrations.length === 1 ? "" : "s"} this release ships
                </summary>
                <ul className="mt-2 space-y-0.5 font-mono text-[11.5px] text-muted">
                  {detail.release.migrations.map((migration) => (
                    <li key={migration} data-release-migration={migration}>
                      {migration}
                    </li>
                  ))}
                </ul>
              </details>
            ) : null}

            {detail.release.notes_md ? (
              <details className="mt-3">
                <summary className="cursor-pointer text-[12.5px] text-muted">
                  Release notes as the publisher wrote them
                </summary>
                <pre
                  className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap rounded-lg bg-quiet-soft/50 p-3 text-[11.5px]"
                  data-release-notes
                >
                  {detail.release.notes_md}
                </pre>
              </details>
            ) : null}
          </section>

          <section className="rounded-xl border border-line">
            <header className="border-b border-line px-4 py-3">
              <h2 className="text-[13.5px] font-medium">
                Artifacts ({detail.artifacts.length})
              </h2>
            </header>
            <ArtifactTable
              artifacts={detail.artifacts}
              emptyHint={`The manifest for ${detail.release.version} is cached, but no artifact row arrived with it. A manifest whose artifacts are missing is a fetch that half-completed, not an empty release.`}
            />
            {detail.missing_kinds.length > 0 ? (
              <ul className="space-y-1 border-t border-line px-4 py-3" data-missing-kinds>
                {detail.missing_kinds.map((kind) => (
                  <li
                    key={kind}
                    className="flex items-center gap-1.5 text-[12px] text-caution"
                    data-missing-kind={kind}
                  >
                    <FileWarning className="size-3.5 shrink-0" aria-hidden="true" />
                    {kind} — not published for {detail.release.version}
                  </li>
                ))}
              </ul>
            ) : null}
          </section>
        </>
      ) : null}
    </div>
  );
}
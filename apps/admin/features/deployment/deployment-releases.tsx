"use client";

/**
 * `/deployment/releases` and `/deployment/releases/{version}` — the release browser and
 * `View Changes` (REQ-024, slice 1).
 *
 * Two screens in one file because they are one decision read two ways, and the decision is the
 * API's: a release is a row here and a page there, and the *same* `is_available` flag decides
 * whether `Deploy` is offered. A browser that rendered its own notion of "the latest release"
 * would drift from the card the moment a channel or a core version excluded the newest one.
 *
 * The channel chips **narrow only**, and the API decides what a refusal looks like: asking a
 * stable installation for the nightly list returns the stable list, because the alternative —
 * showing a nightly next to a stable install — is how an upgrade to a preview build starts.
 */

import { useCallback, useEffect, useState } from "react";

import { ArrowLeft, FileCode2, RefreshCw, TriangleAlert } from "lucide-react";
import Link from "next/link";
import { useParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError, fetchDeploymentRelease, fetchDeploymentReleases } from "@/lib/api";
import { formatTimestamp } from "@/lib/format";
import type { DeploymentReleaseDetail, DeploymentReleasesResponse } from "@/lib/types";

import { StaleBanner } from "./deployment-parts";

/** The three channels the deployment code knows, as chips. */
const CHANNELS = ["stable", "beta", "nightly"] as const;

/** The release browser. */
export function DeploymentReleases() {
  const [channel, setChannel] = useState<string>("");
  const [data, setData] = useState<DeploymentReleasesResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setData(await fetchDeploymentReleases(channel ? { channel } : {}));
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The release list could not be read.",
      );
    } finally {
      setLoading(false);
    }
  }, [channel]);

  useEffect(() => {
    void load();
  }, [load]);

  return (
    <div className="flex flex-col gap-4">
      {data?.stale_banner ? <StaleBanner text={data.stale_banner} /> : null}

      <div className="flex flex-wrap items-center gap-2">
        {/* `All` means "the installation's own channel", not "every channel": the API resolves
            an empty chip that way, and sending no channel is what the card itself is computed
            from. */}
        <ChannelChip label="This installation" active={channel === ""} onClick={() => setChannel("")} />
        {CHANNELS.map((name) => (
          <ChannelChip
            key={name}
            label={name}
            active={channel === name}
            onClick={() => setChannel(name)}
          />
        ))}
        <button
          type="button"
          onClick={() => void load()}
          className="ml-auto inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] font-medium hover:bg-quiet-soft"
        >
          <RefreshCw aria-hidden="true" className="size-3.5" />
          Refresh
        </button>
      </div>

      {error ? (
        <p role="alert" className="text-[12.5px] text-red-700 dark:text-red-300">
          {error}
        </p>
      ) : null}

      {loading ? (
        <LoadingTable columns={4} rows={5} />
      ) : data && data.empty ? (
        <div className="rounded-xl border border-line bg-surface">
          <EmptyState
            title={`No releases are cached for the ${data.channel} channel`}
            hint="The update check fills this from the release manifest feed. Until one has run, the list is empty — nothing is being shown from memory."
          />
        </div>
      ) : (
        <div className="overflow-x-auto rounded-xl border border-line">
          <table className="w-full border-collapse text-left text-[13px]">
            <thead>
              <tr className="border-b border-line text-[11px] tracking-wide text-muted uppercase">
                <th scope="col" className="px-4 py-3 font-medium">Version</th>
                <th scope="col" className="px-4 py-3 font-medium">Released</th>
                <th scope="col" className="px-4 py-3 font-medium">Notes</th>
                <th scope="col" className="px-4 py-3 font-medium">Migrations</th>
                <th scope="col" className="px-4 py-3 font-medium">Channel</th>
              </tr>
            </thead>
            <tbody>
              {data?.releases.map((release) => (
                <tr key={`${release.channel}-${release.version}`} className="border-b border-line last:border-b-0">
                  <td className="px-4 py-3">
                    <Link
                      href={`/deployment/releases/${encodeURIComponent(release.version)}?channel=${encodeURIComponent(release.channel)}`}
                      className="inline-flex items-center gap-1.5 font-medium hover:underline"
                    >
                      {release.version}
                      {release.is_available ? (
                        <span className="rounded-full border border-emerald-500/30 bg-emerald-500/10 px-1.5 py-0.5 text-[10.5px] text-emerald-700 dark:text-emerald-300">
                          on offer
                        </span>
                      ) : null}
                    </Link>
                    {release.breaking ? (
                      <span className="mt-1 flex items-center gap-1 text-[11.5px] text-amber-700 dark:text-amber-300">
                        <TriangleAlert aria-hidden="true" className="size-3" />
                        breaking changes
                      </span>
                    ) : null}
                  </td>
                  <td className="px-4 py-3 text-muted">
                    {release.released_at ?? "not dated by the feed"}
                  </td>
                  <td className="max-w-md px-4 py-3 text-muted">
                    <span className="line-clamp-2">
                      {release.notes || "The feed carried no notes for this release."}
                    </span>
                  </td>
                  <td className="px-4 py-3 text-muted tabular-nums">
                    {release.migrations.length === 0 ? "—" : release.migrations.length}
                  </td>
                  <td className="px-4 py-3 text-muted">{release.channel}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}

/** One channel chip. */
function ChannelChip({
  label,
  active,
  onClick,
}: {
  label: string;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      className={`rounded-lg border px-3 py-1.5 text-[12.5px] font-medium ${
        active
          ? "border-accent bg-accent/10 text-ink"
          : "border-line text-muted hover:bg-quiet-soft"
      }`}
    >
      {label}
    </button>
  );
}

/** `/deployment/releases/{version}` — `View Changes`. */
export function DeploymentReleaseDetailView() {
  const params = useParams<{ version: string }>();
  const [channel, setChannel] = useState<string>("");
  const [data, setData] = useState<DeploymentReleaseDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const version = Array.isArray(params.version) ? params.version[0] : params.version;

  const load = useCallback(async () => {
    if (!version) return;
    setLoading(true);
    setError(null);
    try {
      setData(await fetchDeploymentRelease(version, channel || undefined));
    } catch (caught) {
      setError(
        caught instanceof ApiError
          ? caught.message
          : "That release could not be read from the cache.",
      );
    } finally {
      setLoading(false);
    }
  }, [version, channel]);

  useEffect(() => {
    void load();
  }, [load]);

  // The channel lives in the query string so the link from the list carries it: a release
  // version is unique per *channel* in practice, and opening the detail without it would look
  // the release up on the wrong channel and answer 404 for something that exists.
  useEffect(() => {
    const search = new URLSearchParams(window.location.search);
    const fromUrl = search.get("channel");
    if (fromUrl && fromUrl !== channel) setChannel(fromUrl);
    // Reading the URL once on mount is deliberate: a later change to it is this screen's own
    // chip writing a new URL, and reacting to that would re-render in a loop.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  if (loading) return <LoadingTable columns={2} rows={4} />;

  if (error) {
    return (
      <div className="flex flex-col items-start gap-3">
        <p role="alert" className="text-[13.5px] font-medium text-red-800 dark:text-red-200">
          {error}
        </p>
        <Link
          href="/deployment/releases"
          className="inline-flex items-center gap-1.5 text-[12.5px] text-muted hover:text-ink"
        >
          <ArrowLeft aria-hidden="true" className="size-3.5" />
          Back to releases
        </Link>
      </div>
    );
  }

  if (!data) return null;
  const { release, summary, upgrade_from } = data;

  return (
    <div className="flex flex-col gap-5">
      {summary.stale_banner ? <StaleBanner text={summary.stale_banner} /> : null}

      <div className="flex flex-wrap items-center gap-3">
        <Link
          href="/deployment/releases"
          className="inline-flex items-center gap-1.5 text-[12.5px] text-muted hover:text-ink"
        >
          <ArrowLeft aria-hidden="true" className="size-3.5" />
          Releases
        </Link>
        <h2 className="text-[17px] font-medium tabular-nums">{release.version}</h2>
        {release.breaking ? (
          <span className="inline-flex items-center gap-1 rounded-full border border-amber-500/40 bg-amber-500/10 px-2 py-0.5 text-[11px] text-amber-700 dark:text-amber-300">
            <TriangleAlert aria-hidden="true" className="size-3" />
            breaking changes
          </span>
        ) : null}
      </div>

      <div className="grid grid-cols-1 gap-4 lg:grid-cols-3">
        <section className="lg:col-span-2">
          <h3 className="mb-2 text-[13px] font-medium">What changed</h3>
          {release.notes ? (
            <p className="rounded-xl border border-line bg-surface px-4 py-3.5 text-[13px] leading-relaxed whitespace-pre-wrap">
              {release.notes}
            </p>
          ) : (
            <div className="rounded-xl border border-line bg-surface">
              <EmptyState
                title="The feed carried no notes for this release"
                hint="A release without notes is a real answer, not a missing screen: the publisher published a version and described nothing."
              />
            </div>
          )}
        </section>

        <section className="flex flex-col gap-3">
          <h3 className="text-[13px] font-medium">Compatibility</h3>
          <dl className="flex flex-col gap-2 rounded-xl border border-line bg-surface px-4 py-3.5 text-[12.5px]">
            <Row label="Channel" value={release.channel} />
            <Row
              label="Released"
              value={release.released_at ?? "not dated by the feed"}
            />
            <Row label="Cached" value={formatTimestamp(release.checked_at)} />
            <Row
              label="Minimum core"
              value={release.core_min ?? "not declared"}
            />
            <Row
              label="Checksum"
              value={release.artifact_checksum ?? "not published"}
              mono
            />
          </dl>

          <h3 className="mt-1 text-[13px] font-medium">
            Migrations ({release.migrations.length})
          </h3>
          {release.migrations.length === 0 ? (
            <p className="rounded-xl border border-line bg-surface px-4 py-3 text-[12.5px] text-muted">
              This release ships no migrations.
            </p>
          ) : (
            <ol className="flex flex-col gap-1 rounded-xl border border-line bg-surface px-4 py-3 text-[12.5px]">
              {release.migrations.map((name, index) => (
                <li key={name} className="flex items-center gap-2">
                  <span className="w-5 shrink-0 text-muted tabular-nums">{index + 1}</span>
                  <span className="inline-flex items-center gap-1.5 font-mono text-[12px]">
                    <FileCode2 aria-hidden="true" className="size-3.5 text-muted" />
                    {name}
                  </span>
                </li>
              ))}
            </ol>
          )}

          {/* The deploy action, and the reason it is not live. `upgrade_from` is set only when
              the API says this release is the card's offer, so an older version opened before a
              rollback offers nothing here — deploying to what already runs is a no-op with a
              migrate step attached. */}
          <div className="rounded-xl border border-line bg-surface px-4 py-3.5">
            {upgrade_from ? (
              <p className="text-[12.5px]">
                This is the release the cards are offering, upgrading from{" "}
                <span className="font-medium tabular-nums">{upgrade_from}</span>. The deploy
                wizard arrives with slice 2.
              </p>
            ) : (
              <p className="text-[12.5px] text-muted">
                This release is not on offer to this installation, so there is nothing to deploy
                from this page.
              </p>
            )}
          </div>
        </section>
      </div>
    </div>
  );
}

/** One label/value line in a details list. */
function Row({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <dt className="shrink-0 text-muted">{label}</dt>
      <dd
        className={`min-w-0 text-right break-words ${mono ? "font-mono text-[11.5px]" : ""}`}
      >
        {value}
      </dd>
    </div>
  );
}

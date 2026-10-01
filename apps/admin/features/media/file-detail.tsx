"use client";

/**
 * The file detail screen (docs/requests/REQ-010, slices 2 and 4): preview on the left, tabs on
 * the right.
 *
 * One file, one screen, and the things an editor needs about it: what it is (the preview and
 * its facts), what happened to it (the version history), what is written about it (the
 * metadata), who outside the platform can fetch it (the share links), who inside it can (the
 * permissions), where the site uses it (usage) and what has been done to it (activity).
 *
 * The last two arrived with slice 4, which is what finally made the `media_references` table
 * readable: until then the purge could refuse a file for ever and no screen could say why.
 *
 * The version list is not decoration. Replacing a file writes a new version and restoring an old
 * one appends a *new* version rather than rewriting history, so this screen has to show both, and
 * it has to be able to preview each one — otherwise "restore" is a button that throws bytes away.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  ArrowLeft,
  Check,
  Clock,
  Copy,
  Download,
  History,
  Info,
  Link as LinkIcon,
  Link2,
  Plus,
  RotateCcw,
  ShieldCheck,
  ShieldOff,
  Tag as TagIcon,
  X,
} from "lucide-react";
import Link from "next/link";
import { useParams, useRouter } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { FilePreview, formatDuration, previewKind } from "@/features/media/file-preview";
import { ScanBadge } from "@/features/media/media-shared";
import { GrantsTab } from "@/features/media/grants-tab";
import { MetadataPairsEditor } from "@/features/media/metadata-pairs";
import { SharesTab } from "@/features/media/shares-tab";
import { ActivityTab } from "@/features/media/activity-tab";
import { UsageTab } from "@/features/media/usage-tab";
import {
  ApiError,
  createMediaVersion,
  fetchMediaFile,
  fetchMediaVersions,
  mediaRawUrl,
  mediaVersionRawUrl,
  restoreMediaVersion,
  setMediaHold,
  updateMediaFile,
} from "@/lib/api";
import { formatBytes, formatTimestamp } from "@/lib/format";
import type { MediaExif, MediaFile, MediaVersion, MediaVersionList } from "@/lib/types";

/** Which tab of the right-hand panel is on screen. */
type Tab = "metadata" | "versions" | "permissions" | "shares" | "usage" | "activity";

/** The file detail screen. */
export function MediaFileDetail() {
  const params = useParams<{ id: string }>();
  const router = useRouter();
  const fileId = params?.id ?? "";

  const [file, setFile] = useState<MediaFile | null>(null);
  const [history, setHistory] = useState<MediaVersionList | null>(null);
  const [tab, setTab] = useState<Tab>("metadata");
  /** Which version the preview shows; `null` means the current one. */
  const [previewVersion, setPreviewVersion] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const versionInput = useRef<HTMLInputElement>(null);

  const reload = useCallback(async () => {
    if (!fileId) {
      return;
    }
    setError(null);
    try {
      const [detail, versions] = await Promise.all([
        fetchMediaFile(fileId),
        fetchMediaVersions(fileId),
      ]);
      setFile(detail);
      setHistory(versions);
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : "This file could not be loaded.");
    }
  }, [fileId]);

  useEffect(() => {
    void reload();
  }, [reload]);

  // The preview follows the file: a replace that changes the content type must not leave a stale
  // renderer on screen, and the oldest version the user was looking at may no longer be the one
  // the panel is offering.
  useEffect(() => {
    setPreviewVersion(null);
  }, [file?.id]);

  const shownVersion: MediaVersion | null = useMemo(() => {
    if (!history || previewVersion === null) {
      return null;
    }
    return history.versions.find((entry) => entry.version === previewVersion) ?? null;
  }, [history, previewVersion]);

  const previewSrc = shownVersion
    ? mediaVersionRawUrl(fileId, shownVersion.version)
    : file
      ? mediaRawUrl(file.id)
      : "";

  const onReplace = useCallback(
    async (picked: File, note: string) => {
      setBusy(true);
      setError(null);
      try {
        const answer = await createMediaVersion(fileId, picked, note);
        setFile(answer.file);
        // The history has to be re-read rather than appended locally: the number the database
        // assigned is the truth, and a local guess is how a panel ends up showing version 3 of 2.
        setHistory(await fetchMediaVersions(fileId));
        setPreviewVersion(null);
        setNotice(
          `Version ${answer.version.version} is now current. Version ${
            answer.version.version - 1
          } is still downloadable.`,
        );
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : "The replacement was refused.");
      } finally {
        setBusy(false);
      }
    },
    [fileId],
  );

  const onRestore = useCallback(
    async (version: number) => {
      setBusy(true);
      setError(null);
      try {
        const answer = await restoreMediaVersion(fileId, version);
        setFile(answer.file);
        setHistory(await fetchMediaVersions(fileId));
        setPreviewVersion(null);
        setNotice(
          `Version ${version} is back, as new version ${answer.version.version}. The version it came from is unchanged.`,
        );
      } catch (cause) {
        setError(cause instanceof ApiError ? cause.message : "The restore was refused.");
      } finally {
        setBusy(false);
      }
    },
    [fileId],
  );

  if (error && !file) {
    return (
      <div className="space-y-4">
        <BackLink />
        <EmptyState
          title="This file could not be opened"
          hint={error}
          action={
            <button
              type="button"
              onClick={() => router.push("/media")}
              className="rounded-md bg-ink px-3 py-1.5 text-[12px] font-medium text-inverted transition-opacity hover:opacity-90"
            >
              Back to the library
            </button>
          }
        />
      </div>
    );
  }

  if (!file) {
    return (
      <div className="space-y-4">
        <BackLink />
        <LoadingTable rows={5} columns={2} />
      </div>
    );
  }

  const kind = previewKind(shownVersion?.content_type ?? file.content_type);

  return (
    <div className="space-y-4">
      <BackLink />

      <header className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h2 className="truncate text-[15px] font-semibold text-ink" data-testid="media-file-name">
            {file.filename}
          </h2>
          <p className="mt-0.5 flex flex-wrap items-center gap-2 text-[12px] text-muted">
            <span>{file.content_type}</span>
            <span aria-hidden>·</span>
            <span>{formatBytes(file.size_bytes)}</span>
            <span aria-hidden>·</span>
            <ScanBadge status={file.scan_status} />
            {file.version_count > 1 ? (
              <span
                data-testid="media-file-version-count"
                className="inline-flex items-center gap-1 rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] font-medium text-muted"
              >
                <History className="h-3 w-3" aria-hidden />
                {file.version_count} versions
              </span>
            ) : null}
          </p>
        </div>

        <div className="flex items-center gap-2">
          <input
            ref={versionInput}
            type="file"
            id="media-new-version"
            className="hidden"
            onChange={(event) => {
              const picked = event.target.files?.[0];
              event.target.value = "";
              if (picked) {
                void onReplace(picked, "");
              }
            }}
          />
          <button
            type="button"
            id="media-new-version-button"
            disabled={busy}
            onClick={() => versionInput.current?.click()}
            className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[12px] font-medium text-inverted transition-opacity hover:opacity-90 disabled:opacity-50"
          >
            <Plus className="h-3.5 w-3.5" aria-hidden />
            New version
          </button>
          <a
            href={previewSrc}
            download={file.filename}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12px] font-medium text-ink transition-colors hover:bg-quiet-soft"
          >
            <Download className="h-3.5 w-3.5" aria-hidden />
            Download
          </a>
        </div>
      </header>

      {notice ? (
        <p
          data-testid="media-file-notice"
          className="flex items-start gap-2 rounded-md border border-line bg-quiet-soft px-3 py-2 text-[12px] text-ink"
        >
          <Check className="mt-0.5 h-3.5 w-3.5 shrink-0 text-positive" aria-hidden />
          <span className="flex-1">{notice}</span>
          <button
            type="button"
            onClick={() => setNotice(null)}
            aria-label="Dismiss"
            className="text-muted transition-colors hover:text-ink"
          >
            <X className="h-3.5 w-3.5" aria-hidden />
          </button>
        </p>
      ) : null}

      {error ? (
        <p
          data-testid="media-file-error"
          className="rounded-md border border-negative/30 bg-negative-soft px-3 py-2 text-[12px] text-negative"
        >
          {error}
        </p>
      ) : null}

      <div className="grid gap-4 lg:grid-cols-[minmax(0,1.35fr)_minmax(0,1fr)]">
        {/* The preview, with the version banner above it when an old one is on screen. */}
        <div className="flex min-h-96 flex-col gap-2 rounded-lg border border-line bg-panel p-3">
          {shownVersion ? (
            <p
              data-testid="media-version-banner"
              className="flex flex-wrap items-center gap-2 rounded-md bg-caution-soft px-2.5 py-1.5 text-[12px] text-caution"
            >
              <span>
                Showing version {shownVersion.version} from{" "}
                {formatTimestamp(shownVersion.created_at)} — the file currently serves version{" "}
                {file.version_count}.
              </span>
              <button
                type="button"
                id="media-version-back-to-current"
                onClick={() => setPreviewVersion(null)}
                className="font-medium underline underline-offset-2"
              >
                Back to the current version
              </button>
            </p>
          ) : null}
          <FilePreview
            file={file}
            version={shownVersion ?? undefined}
            src={previewSrc}
            onVersionClick={() => setPreviewVersion(null)}
          />
        </div>

        {/* The tabs. */}
        <div className="flex min-h-96 flex-col rounded-lg border border-line bg-panel">
          <div className="flex border-b border-line" role="tablist" aria-label="File details">
            <TabButton
              id="media-tab-metadata"
              active={tab === "metadata"}
              onClick={() => setTab("metadata")}
              icon={<Info className="h-3.5 w-3.5" aria-hidden />}
              label="Metadata"
            />
            <TabButton
              id="media-tab-versions"
              active={tab === "versions"}
              onClick={() => setTab("versions")}
              icon={<History className="h-3.5 w-3.5" aria-hidden />}
              label={`Versions${history ? ` (${history.version_total})` : ""}`}
            />
            <TabButton
              id="media-tab-permissions"
              active={tab === "permissions"}
              onClick={() => setTab("permissions")}
              icon={<ShieldCheck className="h-3.5 w-3.5" aria-hidden />}
              label="Permissions"
            />
            <TabButton
              id="media-tab-shares"
              active={tab === "shares"}
              onClick={() => setTab("shares")}
              icon={<Link2 className="h-3.5 w-3.5" aria-hidden />}
              label="Share"
            />
            <TabButton
              id="media-tab-usage"
              active={tab === "usage"}
              onClick={() => setTab("usage")}
              icon={<LinkIcon className="h-3.5 w-3.5" aria-hidden />}
              label="Usage"
            />
            <TabButton
              id="media-tab-activity"
              active={tab === "activity"}
              onClick={() => setTab("activity")}
              icon={<Clock className="h-3.5 w-3.5" aria-hidden />}
              label="Activity"
            />
          </div>

          <div className="min-h-0 flex-1 overflow-auto p-3">
            {tab === "metadata" ? (
              <MetadataTab
                file={file}
                onSaved={(updated) => {
                  setFile(updated);
                  setNotice("The metadata is saved.");
                }}
                onError={setError}
              />
            ) : tab === "versions" ? (
              <VersionsTab
                history={history}
                previewing={previewVersion}
                busy={busy}
                onPreview={setPreviewVersion}
                onRestore={onRestore}
              />
            ) : tab === "permissions" ? (
              <GrantsTab targetKind="file" targetId={fileId} siteId={file.site_id} />
            ) : tab === "usage" ? (
              <UsageTab mediaId={fileId} />
            ) : tab === "activity" ? (
              <ActivityTab mediaId={fileId} />
            ) : (
              <SharesTab mediaId={fileId} />
            )}
          </div>
        </div>
      </div>

      <p className="text-[12px] text-muted">
        Replacing a file keeps the old bytes: published pages keep resolving, and the CDN is told to
        purge the key that stopped being current. A version that is previewed here is the version
        the download link offers.
        {kind === "download" ? " This type has no inline preview on the platform." : ""}
      </p>
    </div>
  );
}

/** The link back to the library, with the folder the file came from. */
function BackLink() {
  return (
    <Link
      href="/media"
      className="inline-flex items-center gap-1.5 text-[12px] font-medium text-muted transition-colors hover:text-ink"
    >
      <ArrowLeft className="h-3.5 w-3.5" aria-hidden />
      Media library
    </Link>
  );
}

/** One tab button of the detail screen. */
function TabButton({
  id,
  active,
  onClick,
  icon,
  label,
}: {
  id: string;
  active: boolean;
  onClick: () => void;
  icon: React.ReactNode;
  label: string;
}) {
  return (
    <button
      type="button"
      id={id}
      role="tab"
      aria-selected={active}
      onClick={onClick}
      className={`inline-flex items-center gap-1.5 border-b-2 px-3 py-2 text-[12px] font-medium transition-colors ${
        active
          ? "border-accent-strong text-ink"
          : "border-transparent text-muted hover:text-ink"
      }`}
    >
      {icon}
      {label}
    </button>
  );
}

/** The metadata tab: what the file is, and everything an editor writes about it. */
function MetadataTab({
  file,
  onSaved,
  onError,
}: {
  file: MediaFile;
  onSaved: (file: MediaFile) => void;
  onError: (message: string | null) => void;
}) {
  const [altText, setAltText] = useState(file.alt_text);
  const [caption, setCaption] = useState(file.caption);
  const [description, setDescription] = useState(file.description);
  const [tags, setTags] = useState(file.tags.join(", "));
  const [saving, setSaving] = useState(false);
  const [copied, setCopied] = useState(false);

  // The form follows the file: a restore that changes the row must repaint the fields, or the
  // panel shows the previous version's alt text next to the new version's picture.
  useEffect(() => {
    setAltText(file.alt_text);
    setCaption(file.caption);
    setDescription(file.description);
    setTags(file.tags.join(", "));
  }, [file.id, file.updated_at, file.alt_text, file.caption, file.description, file.tags]);

  const save = useCallback(async () => {
    setSaving(true);
    onError(null);
    try {
      const updated = await updateMediaFile(file.id, {
        alt_text: altText,
        caption,
        description,
        tags: tags
          .split(",")
          .map((tag) => tag.trim())
          .filter(Boolean),
      });
      onSaved(updated);
    } catch (cause) {
      onError(cause instanceof ApiError ? cause.message : "The metadata was not saved.");
    } finally {
      setSaving(false);
    }
  }, [file.id, altText, caption, description, tags, onSaved, onError]);

  return (
    <div className="space-y-4">
      <dl className="grid grid-cols-2 gap-x-3 gap-y-2.5 text-[12px]">
        <Fact label="Kind" value={file.kind} />
        <Fact label="Size" value={formatBytes(file.size_bytes)} />
        <Fact
          label="Dimensions"
          value={
            file.display_width && file.display_height
              ? `${file.display_width} × ${file.display_height}`
              : "—"
          }
        />
        <Fact
          label="Duration"
          value={file.duration_ms ? formatDuration(file.duration_ms) : "—"}
        />
        <Fact label="Pages" value={file.page_count ? String(file.page_count) : "—"} />
        <Fact label="Uploaded" value={formatTimestamp(file.created_at)} />
        <Fact
          label="Modified"
          value={file.updated_at ? formatTimestamp(file.updated_at) : "—"}
        />
        <div className="col-span-2">
          <dt className="text-muted">Checksum</dt>
          <dd className="mt-0.5 flex items-center gap-2">
            <code
              data-testid="media-file-checksum"
              className="truncate rounded bg-quiet-soft px-1.5 py-0.5 font-mono text-[11px] text-ink"
            >
              {file.checksum}
            </code>
            <button
              type="button"
              id="media-copy-checksum"
              onClick={() => {
                void navigator.clipboard?.writeText(file.checksum);
                setCopied(true);
                window.setTimeout(() => setCopied(false), 1500);
              }}
              className="shrink-0 text-muted transition-colors hover:text-ink"
              aria-label="Copy the checksum"
            >
              {copied ? (
                <Check className="h-3.5 w-3.5 text-positive" aria-hidden />
              ) : (
                <Copy className="h-3.5 w-3.5" aria-hidden />
              )}
            </button>
          </dd>
        </div>
      </dl>

      <CameraBlock exif={file.exif} />

      <div className="space-y-3 border-t border-line pt-3">
        <Field
          id="media-alt-text"
          label="Alt text"
          hint="Read by a screen reader in place of the file. Required for meaningful images."
          value={altText}
          onChange={setAltText}
        />
        <Field
          id="media-caption"
          label="Caption"
          hint="A short line shown under the file where it appears."
          value={caption}
          onChange={setCaption}
        />
        <Field
          id="media-description"
          label="Description"
          hint="The longer text, for licensing, credits and context."
          value={description}
          onChange={setDescription}
          multiline
        />
        <Field
          id="media-tags"
          label="Tags"
          hint="Comma separated. Tags are what the browser's tag filter and the search index read."
          value={tags}
          onChange={setTags}
        />

        <button
          type="button"
          id="media-save-metadata"
          disabled={saving}
          onClick={() => void save()}
          className="inline-flex items-center gap-1.5 rounded-md bg-ink px-3 py-1.5 text-[12px] font-medium text-inverted transition-opacity hover:opacity-90 disabled:opacity-50"
        >
          <TagIcon className="h-3.5 w-3.5" aria-hidden />
          {saving ? "Saving…" : "Save metadata"}
        </button>
      </div>

      {/* Its own block and its own save button: the pairs are a *set*, so a caption edit must not
          quietly rewrite them, and a "Save metadata" that also emptied the pairs would be a
          button with two meanings. */}
      <MetadataPairsEditor file={file} onSaved={onSaved} />

      <LegalHold file={file} />
    </div>
  );
}

/**
 * The legal hold, on the metadata tab rather than a fifth tab of its own.
 *
 * A hold is a fact about the *file*, so it belongs with the file's other facts rather than in
 * a place an operator has to go looking. Three things on it are deliberate:
 *
 * - **the reason is required, and the input is always visible.** A hold with no reason is a
 *   file nobody will ever be allowed to delete and nobody can explain; a release with no
 *   reason is a file somebody bypassed the rules for. The platform refuses both.
 * - **the consequence is stated before the button, not after.** "Retention will not remove
 *   this file" is what somebody needs to know at the moment they decide, and discovering it
 *   afterwards — when the file is still there — is the only way to be sure of it.
 * - **a second identical press is a no-op and says so**, rather than writing a second audit
 *   row: a log with three identical entries reads as three people and answers nothing about
 *   who decided what.
 */
function LegalHold({ file }: { file: MediaFile }) {
  const [reason, setReason] = useState("");
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<string | null>(null);
  const [problem, setProblem] = useState<string | null>(null);
  const held = file.legal_hold === true;

  const apply = useCallback(
    async (next: boolean) => {
      setBusy(true);
      setProblem(null);
      setNote(null);
      try {
        const answer = await setMediaHold(file.id, next, reason.trim());
        if (!answer.changed) {
          setNote(`This file is already ${next ? "under a legal hold" : "not held"}. Nothing was recorded.`);
        } else {
          setReason("");
          setNote(
            next
              ? "The file is under a legal hold. No retention run will remove it until the hold is cleared."
              : "The hold is cleared. The next eligible run may remove this file if it is past its window.",
          );
        }
      } catch (cause) {
        setProblem(cause instanceof ApiError ? cause.message : "The hold was not changed.");
      } finally {
        setBusy(false);
      }
    },
    [file.id, reason],
  );

  return (
    <div
      className="mt-3 space-y-2 border-t border-line pt-3"
      data-testid="media-legal-hold"
      data-held={held ? "true" : "false"}
    >
      <h3 className="text-[12.5px] font-medium">Legal hold</h3>
      <p className="text-[12px] text-muted">
        {held
          ? "This file is held: retention will not remove it, at any depth, under any policy. It survives a purge, an emptied trash and a shortened window until somebody clears this."
          : "A legal hold stops every retention rule from touching this file — versions, trash and purge. It is the switch to use for a file under litigation, an audit or a dispute, and clearing it is a decision somebody will be asked to justify."}
      </p>

      <label className="block text-[12px]">
        <span className="text-ink">Reason</span>
        <input
          value={reason}
          onChange={(event) => setReason(event.target.value)}
          placeholder={held ? "Why the hold is being cleared" : "Litigation hold, case 2026-114"}
          data-testid="media-hold-reason"
          className="mt-0.5 w-full rounded-md border border-line bg-canvas px-2 py-1.5 text-[12px]"
        />
      </label>

      {problem ? (
        <p role="alert" className="text-[11.5px] text-danger">
          {problem}
        </p>
      ) : null}
      {note ? (
        <p role="status" className="text-[11.5px] text-muted">
          {note}
        </p>
      ) : null}

      <button
        type="button"
        disabled={busy}
        onClick={() => void apply(!held)}
        data-testid="media-hold-toggle"
        className={[
          "inline-flex items-center gap-1.5 rounded-md border px-2.5 py-1.5 text-[12px] transition-opacity disabled:opacity-50",
          held
            ? "border-warn/50 text-warn hover:bg-warn/5"
            : "border-line hover:bg-canvas",
        ].join(" ")}
      >
        {held ? <ShieldOff className="h-3.5 w-3.5" aria-hidden /> : <ShieldCheck className="h-3.5 w-3.5" aria-hidden />}
        {held ? "Clear the hold" : "Put this file under a legal hold"}
      </button>
    </div>
  );
}

/** The version tab: the history, each entry previewable and restorable. */
function VersionsTab({
  history,
  previewing,
  busy,
  onPreview,
  onRestore,
}: {
  history: MediaVersionList | null;
  previewing: number | null;
  busy: boolean;
  onPreview: (version: number | null) => void;
  onRestore: (version: number) => void;
}) {
  if (!history) {
    return <LoadingTable rows={3} columns={3} />;
  }
  if (history.versions.length === 0) {
    return (
      <EmptyState
        title="No versions recorded"
        hint="This file has no version history. Replacing it starts one."
      />
    );
  }

  return (
    <ul data-testid="media-version-list" className="space-y-2">
      {history.versions.map((version) => {
        const shown = previewing === version.version;
        return (
          <li
            key={version.id}
            data-testid={`media-version-${version.version}`}
            className={`rounded-md border p-2.5 ${
              shown ? "border-accent-strong bg-accent-soft/30" : "border-line"
            }`}
          >
            <div className="flex items-start justify-between gap-2">
              <div className="min-w-0">
                <p className="flex items-center gap-1.5 text-[12px] font-medium text-ink">
                  Version {version.version}
                  {version.is_current ? (
                    <span className="rounded-full bg-positive-soft px-1.5 py-0.5 text-[10px] font-medium text-positive">
                      current
                    </span>
                  ) : null}
                </p>
                <p className="mt-0.5 text-[11px] text-muted">
                  {formatTimestamp(version.created_at)} · {formatBytes(version.size_bytes)}
                  {version.width && version.height ? ` · ${version.width} × ${version.height}` : ""}
                </p>
                {version.note ? (
                  <p className="mt-1 text-[11px] text-muted">{version.note}</p>
                ) : null}
                <code className="mt-1 block truncate font-mono text-[10px] text-muted">
                  {version.checksum}
                </code>
              </div>
              <div className="flex shrink-0 flex-col gap-1">
                <button
                  type="button"
                  onClick={() => onPreview(shown ? null : version.version)}
                  className="rounded-md border border-line px-2 py-1 text-[11px] font-medium text-ink transition-colors hover:bg-quiet-soft"
                >
                  {shown ? "Hide" : "Preview"}
                </button>
                {!version.is_current ? (
                  <button
                    type="button"
                    id={`media-version-restore-${version.version}`}
                    disabled={busy}
                    onClick={() => onRestore(version.version)}
                    title="Bring this version back as a new version. The version you restore is not rewritten."
                    className="inline-flex items-center justify-center gap-1 rounded-md border border-line px-2 py-1 text-[11px] font-medium text-ink transition-colors hover:bg-quiet-soft disabled:opacity-50"
                  >
                    <RotateCcw className="h-3 w-3" aria-hidden />
                    Restore
                  </button>
                ) : null}
                <a
                  href={version.raw_path}
                  download
                  className="inline-flex items-center justify-center gap-1 rounded-md border border-line px-2 py-1 text-[11px] font-medium text-ink transition-colors hover:bg-quiet-soft"
                >
                  <Download className="h-3 w-3" aria-hidden />
                  Get
                </a>
              </div>
            </div>
          </li>
        );
      })}
    </ul>
  );
}

/** One fact of the metadata list. */
function Fact({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <dt className="text-muted">{label}</dt>
      <dd className="mt-0.5 truncate text-ink">{value}</dd>
    </div>
  );
}

/**
 * What the camera said about its own picture (REQ-010, slice 3).
 *
 * A block rather than three more rows in the facts list above, because a photograph either has a
 * camera record or has none: a screenshot and a text file both produce the empty state, and a
 * list that always shows six empty cells reads as a file with nothing in it.
 *
 * The GPS line is the one that has to say more than a value. The record carries `gps: true` and
 * no coordinates — the platform files the *fact* that a photograph carries a location and keeps
 * the position in the bytes the uploader chose to send — so the line states the omission rather
 * than leaving an editor wondering where the map pin went.
 */
function CameraBlock({ exif }: { exif?: MediaExif | null }) {
  if (!exif || Object.keys(exif).length === 0) {
    return (
      <section aria-labelledby="media-camera-heading" data-testid="media-camera-empty">
        <h3 id="media-camera-heading" className="text-[12px] text-muted">
          Camera
        </h3>
        <p className="mt-1 text-[12px] text-muted">
          This file carries no camera data. Photographs taken on a phone or a camera record it
          automatically; exports, screenshots and text files do not.
        </p>
      </section>
    );
  }

  const body = [exif.make, exif.model].filter(Boolean).join(" ");
  const rows: { label: string; value: string }[] = [];
  if (body) rows.push({ label: "Camera", value: body });
  if (exif.lens) rows.push({ label: "Lens", value: exif.lens });
  if (typeof exif.iso === "number") rows.push({ label: "ISO", value: String(exif.iso) });
  const exposure = formatExposure(exif.exposure_ms);
  if (exposure) rows.push({ label: "Exposure", value: exposure });
  if (typeof exif.aperture_x100 === "number")
    rows.push({ label: "Aperture", value: `f/${(exif.aperture_x100 / 100).toFixed(1)}` });
  if (typeof exif.focal_length_mm === "number")
    rows.push({ label: "Focal length", value: `${exif.focal_length_mm} mm` });
  if (exif.software) rows.push({ label: "Software", value: exif.software });
  if (exif.captured_at)
    rows.push({ label: "Captured", value: formatTimestamp(exif.captured_at) });
  if (typeof exif.orientation === "number" && exif.orientation > 1)
    rows.push({ label: "Rotation", value: rotationLabel(exif.orientation) });

  return (
    <section aria-labelledby="media-camera-heading" data-testid="media-camera-block">
      <h3 id="media-camera-heading" className="text-[12px] text-muted">
        Camera
      </h3>
      <dl className="mt-1.5 grid grid-cols-2 gap-x-3 gap-y-2 text-[12px]">
        {rows.map((row) => (
          <Fact key={row.label} label={row.label} value={row.value} />
        ))}
        {exif.gps ? (
          <div className="col-span-2" data-testid="media-camera-gps">
            <dt className="text-muted">Location</dt>
            <dd className="mt-0.5 text-ink">
              This picture carries a location. Omnion records that it does and does not store the
              coordinates.
            </dd>
          </div>
        ) : null}
      </dl>
    </section>
  );
}

/**
 * A shutter time the way a camera prints it.
 *
 * `1/200 s` rather than `0.005 s` for the fast speeds, and a decimal for the slow ones — the
 * fraction is what somebody comparing two frames recognises, and `0.005 s` is not.
 */
function formatExposure(millis?: number): string | null {
  if (typeof millis !== "number" || millis <= 0) return null;
  if (millis >= 1000) return `${(millis / 1000).toFixed(1)} s`;
  return `1/${Math.max(1, Math.round(1000 / millis))} s`;
}

/** What an EXIF orientation value means in words, for the fields that are not upright. */
function rotationLabel(orientation: number): string {
  const labels: Record<number, string> = {
    2: "Mirrored horizontally",
    3: "Rotated 180°",
    4: "Mirrored vertically",
    5: "Mirrored, then rotated 90°",
    6: "Rotated 90°",
    7: "Mirrored, then rotated 270°",
    8: "Rotated 270°",
  };
  return labels[orientation] ?? `Orientation ${orientation}`;
}

/** One labelled field of the metadata form. */
function Field({
  id,
  label,
  hint,
  value,
  onChange,
  multiline,
}: {
  id: string;
  label: string;
  hint: string;
  value: string;
  onChange: (value: string) => void;
  multiline?: boolean;
}) {
  const shared =
    "mt-1 w-full rounded-md border border-line bg-panel px-2.5 py-1.5 text-[12px] text-ink outline-none transition-colors focus:border-accent-strong";
  return (
    <div>
      <label htmlFor={id} className="block text-[12px] font-medium text-ink">
        {label}
      </label>
      <p className="text-[11px] text-muted">{hint}</p>
      {multiline ? (
        <textarea
          id={id}
          value={value}
          rows={3}
          onChange={(event) => onChange(event.target.value)}
          className={shared}
        />
      ) : (
        <input
          id={id}
          value={value}
          onChange={(event) => onChange(event.target.value)}
          className={shared}
        />
      )}
    </div>
  );
}

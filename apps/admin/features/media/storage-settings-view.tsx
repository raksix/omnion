"use client";

/**
 * Per-site storage settings: where the bytes live (REQ-010, slice 3).
 *
 * The transformation tab answers *what a size is*; this one answers *where the file is*. Three
 * things on it are not decoration, and each exists because the shortcut is wrong:
 *
 * - **the connection test writes.** A test that only reads proves the key can list, and read is
 *   the permission an object store grants most often. The result panel says which it proved —
 *   "reached the bucket and wrote a probe object" and "reached it but could not write" are
 *   different problems with different fixes, and a single green tick would hide the second;
 * - **the test runs against the form, not the row.** The person clicking the button has unsaved
 *   edits in front of them, and a result describing the *saved* configuration is a result about
 *   something they are no longer looking at;
 * - **the visibility choice states its consequence.** `public` means anybody with the URL gets
 *   the file with no session, which is what a public bucket means, and a select whose options
 *   are `public`/`private` says nothing about that.
 */
import { useCallback, useEffect, useState } from "react";

import { CheckCircle2, Loader2, PlugZap, Save, TriangleAlert } from "lucide-react";

import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  fetchMediaStorageSettings,
  saveMediaStorageSettings,
  testMediaStorageConnection,
} from "@/lib/api";
import { useSites } from "@/lib/sites";
import type { MediaStorageProbe, MediaStorageSettings, MediaStorageSettingsInput } from "@/lib/types";

/** The drivers a site can be pointed at. */
const DRIVERS: [string, string][] = [
  ["s3", "S3-compatible object store — MinIO, AWS, R2, Backblaze"],
  ["fs", "Filesystem — the directory this process was started with"],
];

/** The visibility choices, spelled out as the consequence rather than as a word. */
const VISIBILITIES: [string, string][] = [
  ["private", "Private — served through a signed URL that expires"],
  ["public", "Public — anyone holding the file's URL can fetch it, no session"],
];

/** The bounds the API enforces, restated here so the form can refuse before it posts. */
const TTL_RANGE = { min: 60, max: 604_800 } as const;
const UPLOAD_RANGE = { min: 1, max: 1024 } as const;

/** One setting as the editor holds it while it is being changed. */
type Draft = {
  driver: string;
  endpoint: string;
  region: string;
  bucket: string;
  path_prefix: string;
  public_base_url: string;
  signed_url_ttl_seconds: string;
  default_visibility: string;
  max_upload_mb: string;
  allowed_content_types: string;
};

/** The form's draft as a fresh copy of a stored row. */
function toDraft(settings: MediaStorageSettings): Draft {
  return {
    driver: settings.driver,
    endpoint: settings.endpoint,
    region: settings.region,
    bucket: settings.bucket,
    path_prefix: settings.path_prefix,
    public_base_url: settings.public_base_url,
    signed_url_ttl_seconds: String(settings.signed_url_ttl_seconds),
    default_visibility: settings.default_visibility,
    max_upload_mb: String(settings.max_upload_mb),
    allowed_content_types: settings.allowed_content_types.join(", "),
  };
}

/** The per-site storage settings of the selected site. */
export function MediaStorageSettingsView() {
  const { selectedSite, status: sitesStatus } = useSites();
  const [settings, setSettings] = useState<MediaStorageSettings | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [testing, setTesting] = useState(false);
  const [probe, setProbe] = useState<MediaStorageProbe | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  const siteId = selectedSite?.id ?? null;
  const reload = useCallback(() => setReloadToken((token) => token + 1), []);

  useEffect(() => {
    if (!siteId) {
      setSettings(null);
      setDraft(null);
      return;
    }
    let cancelled = false;
    setSettings(null);
    setDraft(null);
    setError(null);
    setProbe(null);
    fetchMediaStorageSettings(siteId)
      .then((answer) => {
        if (cancelled) {
          return;
        }
        setSettings(answer);
        setDraft(toDraft(answer));
      })
      .catch((cause: unknown) => {
        if (!cancelled) {
          setError(cause instanceof Error ? cause.message : "the settings could not be loaded");
        }
      });
    return () => {
      cancelled = true;
    };
  }, [siteId, reloadToken]);

  /** Read the field error the API named, if it named this one. */
  const errorFor = useCallback(
    (field: string) => (fieldError && fieldError.field === field ? fieldError.message : null),
    [fieldError],
  );

  /**
   * Turn the form into a payload, refusing an out-of-range number here.
   *
   * The refusal happens *before* the request: a screen that posts a bad value and then shows a
   * banner has already sent it, and the person is left wondering whether anything changed.
   */
  const build = useCallback((): MediaStorageSettingsInput | { field: string; message: string } => {
    if (!draft) {
      return { field: "driver", message: "the settings have not loaded yet" };
    }
    const number = (raw: string): number => {
      const parsed = Number(raw.trim());
      return Number.isFinite(parsed) ? Math.trunc(parsed) : Number.NaN;
    };
    const ttl = number(draft.signed_url_ttl_seconds);
    if (!Number.isFinite(ttl) || ttl < TTL_RANGE.min || ttl > TTL_RANGE.max) {
      return {
        field: "signed_url_ttl_seconds",
        message: `expected between ${TTL_RANGE.min} and ${TTL_RANGE.max} seconds`,
      };
    }
    const megabytes = number(draft.max_upload_mb);
    if (!Number.isFinite(megabytes) || megabytes < UPLOAD_RANGE.min || megabytes > UPLOAD_RANGE.max) {
      return {
        field: "max_upload_mb",
        message: `expected between ${UPLOAD_RANGE.min} and ${UPLOAD_RANGE.max} MB`,
      };
    }
    if (draft.bucket.trim() === "") {
      return { field: "bucket", message: "the bucket name may not be blank" };
    }
    if (draft.driver === "s3" && !/^https?:\/\/[^/?#]+$/.test(draft.endpoint.trim())) {
      return {
        field: "endpoint",
        message: "expected a bare http or https origin, without a path",
      };
    }
    if (
      draft.public_base_url.trim() !== "" &&
      !/^https?:\/\/\S+$/.test(draft.public_base_url.trim())
    ) {
      return {
        field: "public_base_url",
        message: "expected an http or https URL, or left blank to serve files from the API",
      };
    }
    return {
      driver: draft.driver,
      endpoint: draft.endpoint.trim(),
      region: draft.region.trim(),
      bucket: draft.bucket.trim(),
      path_prefix: draft.path_prefix.trim(),
      public_base_url: draft.public_base_url.trim(),
      signed_url_ttl_seconds: ttl,
      default_visibility: draft.default_visibility,
      max_upload_mb: megabytes,
      allowed_content_types: draft.allowed_content_types
        .split(",")
        .map((value) => value.trim())
        .filter((value) => value.length > 0),
    };
  }, [draft]);

  const save = useCallback(async () => {
    if (!siteId) {
      return;
    }
    const payload = build();
    if ("field" in payload) {
      setFieldError(payload);
      setNotice(null);
      return;
    }
    setBusy(true);
    setFieldError(null);
    setNotice(null);
    setProbe(null);
    try {
      const saved = await saveMediaStorageSettings(siteId, payload);
      setSettings(saved);
      setDraft(toDraft(saved));
      setNotice("Saved. Files already in the library keep their existing object keys.");
    } catch (cause: unknown) {
      // The API names the field it refused. Putting the message under that input is the whole
      // reason the error carries one, and a save and a connection test of the same bad value
      // must agree about which field is wrong.
      if (cause instanceof ApiError) {
        const field = typeof cause.details?.field === "string" ? cause.details.field : "";
        setFieldError({ field, message: cause.message });
        if (field === "") {
          setError(cause.message);
        }
      } else {
        setError(cause instanceof Error ? cause.message : "the settings could not be saved");
      }
    } finally {
      setBusy(false);
    }
  }, [build, siteId]);

  const test = useCallback(async () => {
    if (!siteId) {
      return;
    }
    const payload = build();
    if ("field" in payload) {
      setFieldError(payload);
      setProbe(null);
      return;
    }
    setTesting(true);
    setFieldError(null);
    setProbe(null);
    try {
      // The candidate as typed, not the saved row: the answer has to describe the form the
      // person is looking at, or they will fix the wrong field.
      setProbe(await testMediaStorageConnection(siteId, payload));
    } catch (cause: unknown) {
      // Same field-naming contract as the save: a value the test refuses must be the value the
      // save refuses, and both must point at the same input.
      if (cause instanceof ApiError) {
        const field = typeof cause.details?.field === "string" ? cause.details.field : "";
        setFieldError({ field, message: cause.message });
        if (field === "") {
          setError(cause.message);
        }
      } else {
        setError(cause instanceof Error ? cause.message : "the connection could not be tested");
      }
    } finally {
      setTesting(false);
    }
  }, [build, siteId]);

  if (sitesStatus === "error") {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <div role="alert" className="px-4 py-6 text-center text-[12.5px] text-accent-strong">
          The site list could not be loaded
        </div>
      </div>
    );
  }

  if (sitesStatus === "ready" && !selectedSite) {
    return (
      <div className="rounded-xl border border-line bg-surface">
        <div className="px-4 py-6 text-center text-[12.5px] text-muted">
          No sites yet — storage settings belong to a site.
        </div>
      </div>
    );
  }

  const set = (key: keyof Draft) => (value: string) => {
    setDraft((current) => (current ? { ...current, [key]: value } : current));
  };

  return (
    <div className="overflow-hidden rounded-xl border border-line bg-surface">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-3">
        <div className="flex items-baseline gap-2">
          <h2 className="text-[13.5px] font-medium">Storage</h2>
          {settings ? (
            <span className="text-[12px] text-muted">
              {settings.configured ? "configured" : "platform defaults"}
            </span>
          ) : null}
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={() => void test()}
            disabled={testing || busy || draft === null}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:cursor-not-allowed"
          >
            {testing ? (
              <Loader2 className="size-3.5 animate-spin" aria-hidden />
            ) : (
              <PlugZap className="size-3.5" aria-hidden />
            )}
            Test connection
          </button>
          <button
            type="button"
            onClick={() => void save()}
            disabled={busy || testing || draft === null}
            className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] text-white transition disabled:cursor-not-allowed"
          >
            {busy ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <Save className="size-3.5" aria-hidden />}
            Save
          </button>
        </div>
      </div>

      <p className="border-b border-line bg-canvas/60 px-4 py-2 text-[12px] text-muted">
        Credentials are never stored here. This record points at the bucket; the key that opens it
        is held by the deployment this process runs in, and the connection test uses that same key.
      </p>

      {notice ? (
        <p className="border-b border-line bg-canvas/60 px-4 py-2 text-[12px] text-muted" role="status">
          {notice}
        </p>
      ) : null}

      {error ? (
        <div role="alert" className="flex flex-col items-center gap-3 border-b border-line px-6 py-6 text-center">
          <p className="text-[12.5px] text-accent-strong">{error}</p>
          <button
            type="button"
            onClick={reload}
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          >
            Try again
          </button>
        </div>
      ) : null}

      {!draft ? (
        <LoadingTable columns={3} />
      ) : (
        <>
          <div className="grid gap-3 border-b border-line px-4 py-4 sm:grid-cols-2">
            <StorageField label="Driver" hint="Which kind of store this site's files are written to.">
              <select value={draft.driver} onChange={(event) => set("driver")(event.target.value)} className={storageInputClass(null)}>
                {DRIVERS.map(([value, label]) => (
                  <option key={value} value={value}>
                    {label}
                  </option>
                ))}
              </select>
            </StorageField>

            <StorageField label="Bucket" hint="The bucket this site's objects live in." error={errorFor("bucket")}>
              <input
                value={draft.bucket}
                onChange={(event) => set("bucket")(event.target.value)}
                placeholder="omnion-media"
                className={storageInputClass(errorFor("bucket"))}
              />
            </StorageField>

            <StorageField
              label="Endpoint"
              hint="A bare origin — no path, no query. Ignored for the filesystem driver."
              error={errorFor("endpoint")}
            >
              <input
                value={draft.endpoint}
                onChange={(event) => set("endpoint")(event.target.value)}
                placeholder="https://s3.eu-west-1.amazonaws.com"
                disabled={draft.driver === "fs"}
                className={storageInputClass(errorFor("endpoint"))}
              />
            </StorageField>

            <StorageField label="Region" hint="Signed requests are scoped to it.">
              <input
                value={draft.region}
                onChange={(event) => set("region")(event.target.value)}
                placeholder="us-east-1"
                disabled={draft.driver === "fs"}
                className={storageInputClass(null)}
              />
            </StorageField>

            <StorageField
              label="Key prefix"
              hint="A namespace inside the bucket, so several sites can share one. Empty means the site owns the whole bucket."
              error={errorFor("path_prefix")}
            >
              <input
                value={draft.path_prefix}
                onChange={(event) => set("path_prefix")(event.target.value)}
                placeholder="tenant-a"
                className={storageInputClass(errorFor("path_prefix"))}
              />
            </StorageField>

            <StorageField
              label="Public base URL"
              hint="Where a public file is served from once a CDN or a public bucket is in front. Blank means the API serves it."
              error={errorFor("public_base_url")}
            >
              <input
                value={draft.public_base_url}
                onChange={(event) => set("public_base_url")(event.target.value)}
                placeholder="https://cdn.example.com/media"
                className={storageInputClass(errorFor("public_base_url"))}
              />
            </StorageField>

            <StorageField
              label="Signed URL lifetime"
              hint={`${TTL_RANGE.min}–${TTL_RANGE.max} seconds. A private file is served by a link that expires.`}
              error={errorFor("signed_url_ttl_seconds")}
            >
              <input
                value={draft.signed_url_ttl_seconds}
                onChange={(event) => set("signed_url_ttl_seconds")(event.target.value)}
                inputMode="numeric"
                aria-label="Signed URL lifetime in seconds"
                className={storageInputClass(errorFor("signed_url_ttl_seconds"))}
              />
            </StorageField>

            <StorageField
              label="Maximum upload"
              hint={`${UPLOAD_RANGE.min}–${UPLOAD_RANGE.max} MB, for this site only.`}
              error={errorFor("max_upload_mb")}
            >
              <input
                value={draft.max_upload_mb}
                onChange={(event) => set("max_upload_mb")(event.target.value)}
                inputMode="numeric"
                aria-label="Maximum upload size in megabytes"
                className={storageInputClass(errorFor("max_upload_mb"))}
              />
            </StorageField>

            <StorageField
              label="Default visibility"
              hint="What a new upload gets unless the uploader says otherwise."
            >
              <select
                value={draft.default_visibility}
                onChange={(event) => set("default_visibility")(event.target.value)}
                className={storageInputClass(null)}
              >
                {VISIBILITIES.map(([value, label]) => (
                  <option key={value} value={value}>
                    {label}
                  </option>
                ))}
              </select>
            </StorageField>

            <StorageField
              label="Allowed content types"
              hint="Comma separated. Empty accepts everything the platform allows."
            >
              <input
                value={draft.allowed_content_types}
                onChange={(event) => set("allowed_content_types")(event.target.value)}
                placeholder="image/png, image/jpeg, application/pdf"
                className={storageInputClass(null)}
              />
            </StorageField>
          </div>

          {settings ? (
            <p className="border-b border-line px-4 py-2 text-[12px] text-muted">
              {settings.visibility_note}
            </p>
          ) : null}

          {draft.default_visibility === "public" ? (
            <p
              role="alert"
              className="flex items-start gap-2 border-b border-line bg-amber-50/60 px-4 py-2 text-[12px] text-amber-800"
            >
              <TriangleAlert className="mt-0.5 size-3.5 shrink-0" aria-hidden />
              Public uploads are fetchable by anyone who has the link, with no session and no
              signature. Use it only for a bucket you are certain is world-readable.
            </p>
          ) : null}

          {probe ? (
            <div
              role="status"
              data-testid="media-storage-probe"
              className={[
                "flex items-start gap-2 border-b border-line px-4 py-3 text-[12.5px]",
                probe.ok ? "bg-canvas/60 text-muted" : "text-accent-strong",
              ].join(" ")}
            >
              {probe.ok ? (
                <CheckCircle2 className="mt-0.5 size-3.5 shrink-0" aria-hidden />
              ) : (
                <TriangleAlert className="mt-0.5 size-3.5 shrink-0" aria-hidden />
              )}
              <span>
                {probe.detail}
                {probe.elapsed_ms > 0 ? ` (${probe.elapsed_ms} ms)` : ""}
                {probe.ok ? "" : " — nothing was changed."}
              </span>
            </div>
          ) : null}
        </>
      )}
    </div>
  );
}

/** One labelled form field with its hint and error. */
function StorageField({
  label,
  hint,
  error,
  children,
}: {
  label: string;
  hint: string;
  error?: string | null;
  children: React.ReactNode;
}) {
  return (
    <label className="block">
      <span className="text-[12.5px] font-medium">{label}</span>
      <div className="mt-1">{children}</div>
      {error ? (
        <span className="mt-1 block text-[11.5px] text-accent-strong" role="alert">
          {error}
        </span>
      ) : (
        <span className="mt-1 block text-[11.5px] text-muted">{hint}</span>
      )}
    </label>
  );
}

/** The input class, tinted when the field carries an error. */
function storageInputClass(error: string | null | undefined): string {
  return [
    "w-full rounded-lg border bg-surface px-2.5 py-1.5 text-[13px] outline-none transition",
    "focus:border-accent focus:ring-2 focus:ring-accent/20",
    "disabled:opacity-60",
    error ? "border-accent-strong" : "border-line",
  ].join(" ");
}

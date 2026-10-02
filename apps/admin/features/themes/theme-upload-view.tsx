"use client";

/**
 * `/themes/upload` — the package validation and install screen (REQ-062, slice 3).
 *
 * The criterion is one sentence with three claims in it: "Import validation refuses a package
 * with an unknown slot or an unknown block type and lists each problem; a valid package
 * installs as inactive" and the neighbouring one adds "cannot be activated before installation
 * completes, cannot be deleted while active, and a bundled theme can never be deleted."
 *
 * Each of those is a property of the *server*, and this screen is built so that it cannot
 * accidentally satisfy them on its own:
 *
 *  - **Validate is a different request from install, and the difference is the whole design.**
 *    `POST /api/v1/themes/validate` writes nothing and answers with every finding; `POST
 *    /api/v1/themes/install` refuses while any of them is an error and then writes the row
 *    *inactive*. So a report is shown before anything is written, and the install button is
 *    only offered on a report this screen has read.
 *  - **The report lists findings, not the first one.** A validator that stops at the first
 *    problem makes the operator fix one file, upload it, learn about the next — an afternoon
 *    per mistake. The list is grouped by severity and each row names a *path* into the file
 *    (`slots.header.blocks[2].type`), so a finding points at a line instead of being a sentence
 *    about a sentence.
 *  - **The install lands inactive, and the screen says so before the click, not after.** The
 *    install answer carries `install.active: false` and the note says the theme is in the
 *    gallery with an `Uploaded` tag and nothing renders with it until someone activates it.
 *  - **A bundled theme cannot be deleted and an in-use one cannot either**, so the gallery's
 *    remove control is gated on the server's `canDelete` and the 409's own message is printed
 *    rather than swallowed. `removeTheme` throws on purpose.
 *
 * The file is read in the browser and parsed here, because the API takes *JSON*, not an
 * archive: `MAX_PACKAGE_BYTES` is a 4 MB wall on the request body, and a package is declarative
 * data. A zip would need an extraction step the platform has no reason to have.
 */
import { useCallback, useRef, useState } from "react";

import { AlertTriangle, CheckCircle2, Loader2, Upload, XCircle } from "lucide-react";
import Link from "next/link";

import { EmptyState } from "@/components/empty-state";
import {
  ApiError,
  installThemePackage,
  removeTheme,
  validateThemePackage,
} from "@/lib/api";
import type { ThemePackageFinding, ThemePackageReport } from "@/lib/api";

/**
 * The platform's own cap, mirrored.
 *
 * The server is the authority and refuses over it with its own sentence; this only stops the
 * obvious mistake before a 4 MB body has been built in memory. A package that passes here and
 * is refused there is fine — the two numbers agree — but a package refused here for being
 * *smaller* than the server's limit would be a screen lying about the rule.
 */
const MAX_PACKAGE_BYTES = 4 * 1024 * 1024;

type Parsed = {
  /** The file's own name, for the heading. */
  fileName: string;
  /** The parsed package, the exact body both requests take. */
  package: unknown;
  /** Bytes on disk, shown so the operator can see what they picked. */
  bytes: number;
};

export function ThemeUploadView() {
  const [parsed, setParsed] = useState<Parsed | null>(null);
  const [report, setReport] = useState<ThemePackageReport | null>(null);
  const [installed, setInstalled] = useState<{ themeKey: string; version: string } | null>(null);
  const [busy, setBusy] = useState<"validate" | "install" | "remove" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [parseError, setParseError] = useState<string | null>(null);
  const [removed, setRemoved] = useState<string | null>(null);
  const input = useRef<HTMLInputElement>(null);

  const read = useCallback(async (file: File) => {
    setError(null);
    setParseError(null);
    setReport(null);
    setInstalled(null);
    setRemoved(null);
    if (file.size > MAX_PACKAGE_BYTES) {
      // Named in the operator's units, because "4 MB" is the rule and "4194304" is not an
      // answer to "why did it refuse".
      setParseError(
        `That file is ${(file.size / 1024 / 1024).toFixed(1)} MB and the platform accepts at most 4 MB of package. A package carries declarative data only — manifest, tokens and slot trees — so a larger one is a different kind of file.`,
      );
      return;
    }
    let text: string;
    try {
      text = await file.text();
    } catch (cause: unknown) {
      setParseError(cause instanceof Error ? cause.message : "The file could not be read.");
      return;
    }
    let value: unknown;
    try {
      value = JSON.parse(text);
    } catch (cause: unknown) {
      setParseError(
        cause instanceof Error
          ? `That file is not JSON: ${cause.message}`
          : "That file is not JSON.",
      );
      return;
    }
    setParsed({ fileName: file.name, package: value, bytes: file.size });
  }, []);

  const validate = useCallback(async () => {
    if (!parsed) return;
    setBusy("validate");
    setError(null);
    try {
      setReport(await validateThemePackage(parsed.package));
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The package could not be validated.");
    } finally {
      setBusy(null);
    }
  }, [parsed]);

  const install = useCallback(async () => {
    if (!parsed) return;
    setBusy("install");
    setError(null);
    try {
      const result = await installThemePackage(parsed.package);
      setInstalled({ themeKey: result.install.themeKey, version: result.install.version });
      setReport(result.report);
    } catch (cause: unknown) {
      // A refused install carries the whole report in the envelope, so the screen shows the
      // server's sentences rather than a generic failure — the operator is here precisely
      // because something is wrong with the file.
      setError(cause instanceof ApiError ? cause.message : "The package could not be installed.");
    } finally {
      setBusy(null);
    }
  }, [parsed]);

  const remove = useCallback(async (themeKey: string) => {
    setBusy("remove");
    setError(null);
    try {
      await removeTheme(themeKey);
      setRemoved(`${themeKey} was removed. Nothing rendered with it, or the server would have refused.`);
    } catch (cause: unknown) {
      setError(cause instanceof ApiError ? cause.message : "The theme could not be removed.");
    } finally {
      setBusy(null);
    }
  }, []);

  const findings = report?.findings ?? [];
  const errors = findings.filter((finding) => finding.severity === "error");
  const warnings = findings.filter((finding) => finding.severity === "warning");

  return (
    <div className="flex flex-col gap-4" data-theme-upload>
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div className="min-w-0">
          <h2 className="text-[15px] font-medium">Upload a theme</h2>
          <p className="text-[12.5px] text-muted">
            A package is JSON: a manifest, its tokens and its slot trees. It is validated
            first, and an install lands <strong>inactive</strong> — nothing renders with it until
            an operator activates it from the gallery.
          </p>
        </div>
        <Link
          href="/themes"
          className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
          data-theme-upload-gallery-link
        >
          Back to the gallery
        </Link>
      </header>

      <div className="rounded-xl border border-line bg-surface p-4">
        <label htmlFor="theme-package-file" className="text-[12.5px] font-medium">
          Package file
        </label>
        <p className="mb-2 text-[11.5px] text-muted">
          <code className="font-mono">.json</code>, up to 4 MB. Export one from any site&apos;s
          theme builder.
        </p>
        <input
          ref={input}
          id="theme-package-file"
          type="file"
          accept="application/json,.json"
          data-theme-upload-file
          onChange={(event) => {
            const file = event.target.files?.[0];
            if (file) void read(file);
          }}
          className="block w-full max-w-md text-[12.5px] text-muted file:mr-3 file:rounded-lg file:border file:border-line file:bg-canvas file:px-3 file:py-1.5 file:text-[12.5px] file:text-ink"
        />
        {parsed ? (
          <p className="mt-3 text-[12px] text-muted" data-theme-upload-file-name={parsed.fileName}>
            <CheckCircle2 className="mr-1 inline size-3.5 text-positive" aria-hidden />
            {parsed.fileName} · {(parsed.bytes / 1024).toFixed(1)} KB · read
          </p>
        ) : null}
        {parseError ? (
          <p role="alert" className="mt-3 rounded-lg border border-accent/40 bg-accent-soft px-3 py-2 text-[12.5px] text-accent-strong" data-theme-upload-parse-error>
            {parseError}
          </p>
        ) : null}
      </div>

      {parsed ? (
        <div className="flex flex-wrap items-center gap-2">
          <button
            type="button"
            className="flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
            onClick={() => void validate()}
            disabled={busy !== null}
            data-theme-upload-validate
          >
            {busy === "validate" ? <Loader2 className="size-3.5 animate-spin" aria-hidden /> : <Upload className="size-3.5" aria-hidden />}
            {busy === "validate" ? "Checking…" : "Validate"}
          </button>
          <button
            type="button"
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
            onClick={() => {
              setParsed(null);
              setReport(null);
              setInstalled(null);
              setParseError(null);
              if (input.current) input.current.value = "";
            }}
            disabled={busy !== null}
            data-theme-upload-clear
          >
            Choose another file
          </button>
        </div>
      ) : null}

      {error ? (
        <p role="alert" className="rounded-xl border border-accent/40 bg-accent-soft px-4 py-3 text-[12.5px] text-accent-strong" data-theme-upload-error>
          {error}
        </p>
      ) : null}
      {removed ? (
        <p role="status" className="rounded-xl border border-line bg-surface px-4 py-3 text-[12.5px] text-muted" data-theme-upload-removed>
          {removed}
        </p>
      ) : null}

      {report ? (
        <div className="flex flex-col gap-3" data-theme-upload-report data-theme-upload-valid={report.valid ? "true" : "false"}>
          <header className="rounded-xl border border-line bg-surface px-4 py-3">
            <div className="flex flex-wrap items-center gap-2">
              {report.valid ? (
                <CheckCircle2 className="size-4 text-positive" aria-hidden />
              ) : (
                <XCircle className="size-4 text-accent" aria-hidden />
              )}
              <h3 className="text-[13px] font-medium" data-theme-upload-report-theme={report.themeKey}>
                {report.name || report.themeKey || "This package"}
              </h3>
              <span className="font-mono text-[11.5px] text-muted">
                {report.themeKey} · v{report.version}
              </span>
              <span
                className={`ml-auto rounded-full px-2 py-0.5 text-[11px] ${
                  report.valid ? "bg-positive-soft text-positive" : "bg-accent-soft text-accent-strong"
                }`}
                data-theme-upload-validity
              >
                {report.valid ? "Valid — nothing blocking an install" : `${report.errorCount} problem${report.errorCount === 1 ? "" : "s"}`}
              </span>
            </div>
            <p className="mt-2 text-[12px] text-muted">
              {Object.keys((report.slots ?? {}) as Record<string, unknown>).length} slot
              {Object.keys((report.slots ?? {}) as Record<string, unknown>).length === 1 ? "" : "s"} ·{" "}
              {Object.keys((report.tokens ?? {}) as Record<string, unknown>).length} token
              {Object.keys((report.tokens ?? {}) as Record<string, unknown>).length === 1 ? "" : "s"} ·{" "}
              {warnings.length} warning{warnings.length === 1 ? "" : "s"}
            </p>
          </header>

          {findings.length > 0 ? (
            <ul className="flex flex-col gap-1.5" data-theme-upload-findings data-theme-upload-findings-count={findings.length}>
              {findings.map((finding: ThemePackageFinding) => (
                <li
                  key={`${finding.path}-${finding.message}`}
                  className={`flex items-start gap-2 rounded-lg border px-3 py-2 text-[12.5px] ${
                    finding.severity === "error"
                      ? "border-accent/40 bg-accent-soft text-accent-strong"
                      : "border-caution/40 bg-caution-soft text-caution"
                  }`}
                  data-theme-upload-finding
                  data-theme-upload-finding-path={finding.path}
                  data-theme-upload-finding-severity={finding.severity}
                >
                  <AlertTriangle className="mt-0.5 size-3.5 shrink-0" aria-hidden />
                  <span className="min-w-0">
                    {finding.message}
                    <span className="block font-mono text-[11px] opacity-70">{finding.path}</span>
                  </span>
                </li>
              ))}
            </ul>
          ) : (
            <p className="rounded-xl border border-line bg-surface px-4 py-3 text-[12.5px] text-muted" data-theme-upload-clean>
              Every slot name is one the platform renders and every block type is one it ships.
            </p>
          )}

          {/* The install is offered only on a report this screen has read, and only when that
              report has no errors. The server refuses the same thing — the button is a
              convenience, the refusal is the rule. */}
          <div className="flex flex-wrap items-center gap-2">
            <button
              type="button"
              className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
              onClick={() => void install()}
              disabled={busy !== null || !report.valid}
              data-theme-upload-install
              title={
                report.valid
                  ? "Installs the theme into the gallery, inactive"
                  : "Fix the errors above first — the server would refuse this package"
              }
            >
              {busy === "install" ? "Installing…" : "Install as inactive"}
            </button>
            {report.valid ? (
              <span className="text-[11.5px] text-muted">
                The install writes the theme only. No site starts rendering with it.
              </span>
            ) : null}
          </div>

          {installed ? (
            <p
              role="status"
              className="rounded-xl border border-positive/40 bg-positive-soft px-4 py-3 text-[12.5px] text-positive"
              data-theme-upload-installed
              data-theme-upload-installed-key={installed.themeKey}
            >
              {installed.themeKey} v{installed.version} is in the gallery with an{" "}
              <strong>Uploaded</strong> tag. Nothing renders with it — activate it from{" "}
              <Link className="underline" href="/themes">the gallery</Link> when you want it, and
              the switch is one click back.
            </p>
          ) : null}
        </div>
      ) : null}

      {/* Removal is here and not only on the gallery card, because the criterion is about the
          *refusals*: a bundled theme and an in-use one must both be refused with a sentence
          the operator can read, and the only way to see that is to ask. The key field is a
          deliberate free-text answer — this is the "prove the guard is a gate" control, and a
          dropdown of themes that cannot be deleted would only let you click the ones that
          work. */}
      <div className="rounded-xl border border-line bg-surface p-4">
        <h3 className="text-[13px] font-medium">Remove an uploaded theme</h3>
        <p className="mb-2 text-[11.5px] text-muted">
          A bundled theme can never be removed, and neither can one a site still renders with.
          The server answers <code className="font-mono">theme_bundled_cannot_be_removed</code>{" "}
          or <code className="font-mono">theme_in_use</code>, and this prints its sentence.
        </p>
        <div className="flex flex-wrap items-center gap-2">
          <label htmlFor="theme-remove-key" className="sr-only">
            Theme key
          </label>
          <input
            id="theme-remove-key"
            type="text"
            data-theme-upload-remove-key
            placeholder="theme key, e.g. my-theme"
            className="w-56 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent focus:ring-2 focus:ring-accent/15"
          />
          <button
            type="button"
            className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
            data-theme-upload-remove
            onClick={() => {
              const key = document.getElementById("theme-remove-key");
              if (key instanceof HTMLInputElement && key.value.trim()) {
                void remove(key.value.trim());
              }
            }}
            disabled={busy !== null}
          >
            {busy === "remove" ? "Removing…" : "Remove"}
          </button>
        </div>
      </div>

      {!parsed && !report ? (
        <EmptyState
          title="Nothing picked yet"
          hint="Choose a package file above. Nothing is written until a valid report says so."
        />
      ) : null}
    </div>
  );
}

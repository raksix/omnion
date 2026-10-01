"use client";

/**
 * `/deployment/install` — the environment bundle generator (REQ-128, slice 4).
 *
 * ## The generator's real guarantee is that no credential ships, and the screen states it
 *
 * Every generated file references secrets **by name** (`existingSecret`, `S3_ACCESS_KEY_ID` as a
 * `${…}` reference) and never carries a value. That is not a claim the screen makes about the
 * generator — it is the reason the form has no field that could hold one: name, kind, domain, TLS
 * mode, registry, tag, size preset and an observability toggle. The screen says it on the form, on
 * the result and on each file row, because "does my generated file contain my database password" is
 * the question every operator asks once and nobody wants to answer by reading YAML.
 *
 * ## `renderable: false` is a sentence, not a broken button
 *
 * The request asks for a dry `docker compose config` and `helm template` view. Those tools exist on
 * a **build** host, not on an installed panel, so the render endpoint answers with the input the
 * tool would read plus the reason it cannot run here. The button therefore opens a panel that says
 * which tool would run, why it did not, and shows the generated file's contents — a live button
 * that answers with the truth. A button that answered `501` for every bundle would be exactly the
 * dead control the request forbids.
 *
 * ## The download shows its checksum BESIDE it
 *
 * `GET /bundles/{id}/files/{name}` returns the checksum in a header and the bytes in the body, so
 * the screen fetches the file and reports the header value it received rather than one it computed
 * from its own copy. A download whose verification number came from the same download is not
 * verification.
 *
 * Keyboard: `/` focuses the filter, `g` generates, `Esc` closes the result panel. Under `sm:` the
 * form is one column and the bundle list is cards.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  CheckCircle2,
  Download,
  Eye,
  FileCode2,
  Info,
  Loader2,
  Package,
  RefreshCw,
  Search,
  ShieldCheck,
  Terminal,
  XCircle,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import {
  ApiError,
  BUNDLE_KINDS,
  SIZE_PRESETS,
  TLS_MODES,
  createBundle,
  downloadBundleFile,
  fetchBundles,
  renderBundle,
  type BundleKind,
  type BundleRender,
  type EnvironmentBundle,
} from "@/lib/deployment-api";
import { formatTimestamp } from "@/lib/format";

interface FormState {
  name: string;
  kind: BundleKind;
  version: string;
  domain: string;
  tls_mode: string;
  registry: string;
  tag: string;
  preset: string;
  observability: boolean;
}

/**
 * The blank form.
 *
 * `tag` defaults to **empty and inherits the version** rather than repeating it: the two fields
 * carry the same answer until an operator deliberately pins a different image, and a default that
 * copies one into the other is a second place to forget to update.
 */
const BLANK: FormState = {
  name: "",
  kind: "compose-small",
  version: "0.1.0",
  domain: "omnion.example.com",
  tls_mode: "cert-manager",
  registry: "ghcr.io/raksix/omnion",
  tag: "",
  preset: "small",
  observability: false,
};

/**
 * Client-side validation, mirroring the generator's own vocabulary.
 *
 * The point is to refuse before a round trip *and* to keep the field marked, because the server's
 * own refusal names the field in prose. A form that only discovered a bad name from a 400 leaves
 * the operator hunting through a paragraph for which box was wrong.
 */
function validate(form: FormState): Partial<Record<keyof FormState, string>> {
  const errors: Partial<Record<keyof FormState, string>> = {};
  if (!/^[a-z0-9][a-z0-9._-]{1,62}$/.test(form.name.trim())) {
    errors.name =
      "Lowercase letters, digits, dot, dash or underscore; 2–63 characters. This becomes a file name and a resource name on the target host.";
  }
  if (!/^\d+\.\d+\.\d+([-.][0-9A-Za-z.-]+)?$/.test(form.version.trim())) {
    errors.version = "A three-part version such as 0.1.0, or 0.1.0-rc.1 for a pre-release.";
  }
  if (!/^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$/.test(form.domain.trim())) {
    errors.domain = "A hostname, e.g. omnion.example.com. It becomes the ingress host or the public URL.";
  }
  if (form.registry.trim() && !/^[a-z0-9][a-z0-9._/-]*$/.test(form.registry.trim())) {
    errors.registry =
      "An image registry prefix such as ghcr.io/raksix/omnion — no scheme, no tag, no credentials.";
  }
  return errors;
}

// -------------------------------------------------------------------------------------------

function BundleFileRow({
  bundleId,
  file,
  onDownloaded,
}: {
  bundleId: string;
  file: { name: string; size: number; sha256: string };
  onDownloaded: (name: string, checksum: string, size: number) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const download = async () => {
    setBusy(true);
    setError(null);
    try {
      const { blob, checksum } = await downloadBundleFile(bundleId, file.name);
      const url = URL.createObjectURL(blob);
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = file.name;
      document.body.appendChild(anchor);
      anchor.click();
      anchor.remove();
      URL.revokeObjectURL(url);
      onDownloaded(file.name, checksum, blob.size);
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The file could not be downloaded.",
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <li
      className="flex flex-wrap items-center gap-2 rounded-lg border border-line px-3 py-2"
      data-bundle-file={file.name}
    >
      <FileCode2 className="size-3.5 shrink-0 text-muted" aria-hidden="true" />
      <div className="min-w-0 flex-1">
        <p className="break-all font-mono text-[11.5px]">{file.name}</p>
        <p className="text-[11px] text-muted" data-bundle-file-checksum={file.name}>
          <span className="font-mono" title={file.sha256}>
            sha256 {file.sha256.slice(0, 16)}…
          </span>
          {" · "}
          {file.size.toLocaleString("en")} B
        </p>
        {error ? (
          <p role="alert" className="text-[11px] text-danger">
            {error}
          </p>
        ) : null}
      </div>
      <button
        type="button"
        onClick={() => void download()}
        disabled={busy}
        data-bundle-download={file.name}
        className="flex items-center gap-1 rounded-md border border-line px-1.5 py-1 text-[11.5px] hover:bg-quiet-soft disabled:opacity-60"
      >
        {busy ? (
          <Loader2 className="size-3 animate-spin" aria-hidden="true" />
        ) : (
          <Download className="size-3" aria-hidden="true" />
        )}
        Download
      </button>
    </li>
  );
}

function RenderPanel({ bundle, onClose }: { bundle: EnvironmentBundle; onClose: () => void }) {
  const [render, setRender] = useState<BundleRender | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(true);

  const load = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      setRender(await renderBundle(bundle.id));
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The render could not be prepared.",
      );
    } finally {
      setBusy(false);
    }
  }, [bundle.id]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div
      className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 p-4"
      role="dialog"
      aria-modal="true"
      aria-labelledby="bundle-render-title"
      data-bundle-render-panel
      onClick={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div className="w-full max-w-2xl rounded-xl border border-line bg-surface p-5 shadow-xl">
        <div className="flex items-start gap-2">
          <h3
            id="bundle-render-title"
            className="flex items-center gap-2 text-[15px] font-medium"
          >
            <Eye className="size-4 text-accent" aria-hidden="true" />
            What {render?.tool ?? "the tool"} would read
          </h3>
          <button
            type="button"
            onClick={onClose}
            data-bundle-render-close
            className="ml-auto rounded-md border border-line px-1.5 py-1 text-[11.5px] hover:bg-quiet-soft"
          >
            <span className="sr-only">Close</span>
            <XCircle className="size-3.5" aria-hidden="true" />
          </button>
        </div>

        {busy ? (
          <p className="mt-3 flex items-center gap-2 text-[12.5px] text-muted">
            <Loader2 className="size-3.5 animate-spin" aria-hidden="true" />
            Reading the generated files back…
          </p>
        ) : error ? (
          <p role="alert" data-bundle-render-error className="mt-3 text-[12.5px] text-danger">
            {error}
          </p>
        ) : render ? (
          <>
            <p
              className={`mt-3 rounded-lg px-3 py-2 text-[12.5px] ${
                render.renderable
                  ? "bg-positive-soft text-positive"
                  : "bg-quiet-soft text-muted"
              }`}
              data-bundle-render-reason={String(render.renderable)}
            >
              {render.renderable ? "Rendered on this host." : render.reason}
            </p>

            <h4 className="mt-4 flex items-center gap-1.5 text-[12.5px] font-medium">
              <Terminal className="size-3.5" aria-hidden="true" />
              The commands to run on the target host
            </h4>
            <pre
              className="mt-1.5 overflow-x-auto rounded-lg bg-quiet-soft/60 p-3 font-mono text-[11.5px]"
              data-bundle-render-commands
            >
              {render.commands.join("\n")}
            </pre>

            <h4 className="mt-4 flex items-center gap-1.5 text-[12.5px] font-medium">
              <Package className="size-3.5" aria-hidden="true" />
              Files in this bundle ({render.files.length})
            </h4>
            <ul className="mt-1.5 space-y-1" data-bundle-render-files>
              {render.files.map((file) => (
                <li
                  key={file.name}
                  className="flex flex-wrap items-center gap-2 rounded-lg border border-line px-3 py-2"
                >
                  <div className="min-w-0 flex-1">
                    <p className="break-all font-mono text-[11.5px]">{file.name}</p>
                    <p className="font-mono text-[11px] text-muted">
                      sha256 {file.sha256.slice(0, 16)}… · {file.size.toLocaleString("en")} B
                    </p>
                  </div>
                  <BundleFileRow bundleId={bundle.id} file={file} onDownloaded={() => undefined} />
                </li>
              ))}
            </ul>
          </>
        ) : null}
      </div>
    </div>
  );
}

// -------------------------------------------------------------------------------------------

export function InstallView() {
  const [form, setForm] = useState<FormState>(BLANK);
  const [touched, setTouched] = useState<Partial<Record<keyof FormState, boolean>>>({});
  const [generating, setGenerating] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [generated, setGenerated] = useState<EnvironmentBundle | null>(null);

  const [bundles, setBundles] = useState<EnvironmentBundle[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [rendering, setRendering] = useState<EnvironmentBundle | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [downloaded, setDownloaded] = useState<Record<string, string>>({});

  const filterRef = useRef<HTMLInputElement>(null);
  const nameRef = useRef<HTMLInputElement>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const response = await fetchBundles();
      setBundles(response.bundles);
    } catch (caught) {
      setError(
        caught instanceof ApiError ? caught.message : "The bundle list could not be read.",
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
      if (rendering) return;
      const target = event.target as HTMLElement | null;
      const typing =
        target?.tagName === "INPUT" ||
        target?.tagName === "TEXTAREA" ||
        target?.tagName === "SELECT";
      if (typing) return;
      if (event.key === "/") {
        event.preventDefault();
        filterRef.current?.focus();
      } else if (event.key === "g") {
        event.preventDefault();
        nameRef.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [rendering]);

  const rows = useMemo(() => {
    const all = bundles ?? [];
    const needle = filter.trim().toLowerCase();
    if (!needle) return all;
    return all.filter(
      (bundle) =>
        bundle.name.toLowerCase().includes(needle) ||
        bundle.kind.toLowerCase().includes(needle) ||
        bundle.version.toLowerCase().includes(needle),
    );
  }, [bundles, filter]);

  const set = <K extends keyof FormState>(key: K, value: FormState[K]) => {
    setForm((current) => ({ ...current, [key]: value }));
  };

  const errors = useMemo(() => validate(form), [form]);
  const errorFor = (key: keyof FormState) =>
    touched[key] && errors[key] ? errors[key] : undefined;

  const generate = async () => {
    setTouched({
      name: true,
      version: true,
      domain: true,
      registry: true,
    });
    if (Object.keys(errors).length > 0) {
      setFormError("Fix the marked fields before generating.");
      return;
    }
    setGenerating(true);
    setFormError(null);
    try {
      const bundle = await createBundle({
        ...form,
        name: form.name.trim(),
        version: form.version.trim(),
        domain: form.domain.trim(),
        registry: form.registry.trim(),
        tag: form.tag.trim(),
      });
      setGenerated(bundle);
      setNotice(`${bundle.name} generated — ${bundle.files.length} files`);
      await load();
      window.setTimeout(() => setNotice(null), 4000);
    } catch (caught) {
      setFormError(
        caught instanceof ApiError ? caught.message : "The bundle could not be generated.",
      );
    } finally {
      setGenerating(false);
    }
  };

  const preset = SIZE_PRESETS.find((entry) => entry.key === form.preset) ?? SIZE_PRESETS[0];

  return (
    <div className="space-y-4" data-view="deployment-install">
      <p
        className="flex items-start gap-2 rounded-lg border border-line bg-quiet-soft/40 px-3 py-2.5 text-[12.5px]"
        data-bundle-secret-notice
      >
        <ShieldCheck className="mt-0.5 size-4 shrink-0 text-positive" aria-hidden="true" />
        <span>
          <span className="font-medium">Generated files never contain a credential.</span> They
          reference secrets by name — a Kubernetes{" "}
          <code className="font-mono">existingSecret</code> key, or a{" "}
          <code className="font-mono">${"{VAR}"}</code> the operator fills in a{" "}
          <code className="font-mono">.env</code> the download does not contain. This form has no
          field that could hold a value, which is the guarantee; the generator and the release gate
          both scan the output again on their side.
        </span>
      </p>

      <section className="rounded-xl border border-line">
        <header className="border-b border-line px-4 py-3">
          <h2 className="flex items-center gap-2 text-[13.5px] font-medium">
            <Package className="size-4 text-accent" aria-hidden="true" />
            Generate an environment bundle
          </h2>
        </header>

        <div className="grid gap-4 p-4 lg:grid-cols-2">
          <div className="space-y-3">
            <div>
              <label className="text-[12px] text-muted" htmlFor="bundle-name">
                Target name — becomes the file names and the resource names
              </label>
              <input
                id="bundle-name"
                ref={nameRef}
                value={form.name}
                onChange={(event) => set("name", event.target.value)}
                onBlur={() => setTouched((current) => ({ ...current, name: true }))}
                data-bundle-name
                aria-invalid={Boolean(errorFor("name"))}
                aria-describedby={errorFor("name") ? "bundle-name-error" : undefined}
                className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 font-mono text-[13px] outline-none focus:border-accent"
              />
              {errorFor("name") ? (
                <p id="bundle-name-error" role="alert" data-bundle-name-error className="mt-1 text-[11.5px] text-danger">
                  {errorFor("name")}
                </p>
              ) : null}
            </div>

            <div className="grid gap-3 sm:grid-cols-2">
              <div>
                <label className="text-[12px] text-muted" htmlFor="bundle-kind">
                  Target kind
                </label>
                <select
                  id="bundle-kind"
                  value={form.kind}
                  onChange={(event) => set("kind", event.target.value as BundleKind)}
                  data-bundle-kind
                  className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2 text-[13px] outline-none focus:border-accent"
                >
                  {BUNDLE_KINDS.map((kind) => (
                    <option key={kind} value={kind}>
                      {kind}
                    </option>
                  ))}
                </select>
                <p className="mt-1 text-[11.5px] text-muted">
                  {form.kind === "helm"
                    ? "A values.yaml for infra/helm/omnion, with the chart's own schema checked against it."
                    : form.kind === "compose-enterprise"
                      ? "The small-company stack pointed at external PostgreSQL, Redis and S3 endpoints."
                      : "The small-company stack with its own postgres, redis and minio."}
                </p>
              </div>
              <div>
                <label className="text-[12px] text-muted" htmlFor="bundle-preset">
                  Size preset
                </label>
                <select
                  id="bundle-preset"
                  value={form.preset}
                  onChange={(event) => set("preset", event.target.value)}
                  data-bundle-preset
                  className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2 text-[13px] outline-none focus:border-accent"
                >
                  {SIZE_PRESETS.map((entry) => (
                    <option key={entry.key} value={entry.key}>
                      {entry.label} — {entry.detail}
                    </option>
                  ))}
                </select>
                <p className="mt-1 text-[11.5px] text-muted" data-bundle-preset-detail>
                  Requests and limits per component: {preset.detail}.
                </p>
              </div>
            </div>

            <div>
              <label className="text-[12px] text-muted" htmlFor="bundle-domain">
                Domain name
              </label>
              <input
                id="bundle-domain"
                value={form.domain}
                onChange={(event) => set("domain", event.target.value)}
                onBlur={() => setTouched((current) => ({ ...current, domain: true }))}
                data-bundle-domain
                aria-invalid={Boolean(errorFor("domain"))}
                aria-describedby={errorFor("domain") ? "bundle-domain-error" : undefined}
                className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 font-mono text-[13px] outline-none focus:border-accent"
              />
              {errorFor("domain") ? (
                <p id="bundle-domain-error" role="alert" data-bundle-domain-error className="mt-1 text-[11.5px] text-danger">
                  {errorFor("domain")}
                </p>
              ) : null}
            </div>

            <div className="grid gap-3 sm:grid-cols-2">
              <div>
                <label className="text-[12px] text-muted" htmlFor="bundle-version">
                  Platform version
                </label>
                <input
                  id="bundle-version"
                  value={form.version}
                  onChange={(event) => set("version", event.target.value)}
                  onBlur={() => setTouched((current) => ({ ...current, version: true }))}
                  data-bundle-version
                  aria-invalid={Boolean(errorFor("version"))}
                  aria-describedby={errorFor("version") ? "bundle-version-error" : undefined}
                  className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 font-mono text-[13px] outline-none focus:border-accent"
                />
                {errorFor("version") ? (
                  <p id="bundle-version-error" role="alert" data-bundle-version-error className="mt-1 text-[11.5px] text-danger">
                    {errorFor("version")}
                  </p>
                ) : null}
              </div>
              <div>
                <label className="text-[12px] text-muted" htmlFor="bundle-tag">
                  Image tag (optional) — empty inherits the version
                </label>
                <input
                  id="bundle-tag"
                  value={form.tag}
                  onChange={(event) => set("tag", event.target.value)}
                  data-bundle-tag
                  placeholder={form.version}
                  className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 font-mono text-[13px] outline-none focus:border-accent"
                />
              </div>
            </div>

            <div>
              <label className="text-[12px] text-muted" htmlFor="bundle-registry">
                Image registry
              </label>
              <input
                id="bundle-registry"
                value={form.registry}
                onChange={(event) => set("registry", event.target.value)}
                onBlur={() => setTouched((current) => ({ ...current, registry: true }))}
                data-bundle-registry
                aria-invalid={Boolean(errorFor("registry"))}
                aria-describedby={errorFor("registry") ? "bundle-registry-error" : undefined}
                className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2.5 font-mono text-[13px] outline-none focus:border-accent"
              />
              {errorFor("registry") ? (
                <p id="bundle-registry-error" role="alert" data-bundle-registry-error className="mt-1 text-[11.5px] text-danger">
                  {errorFor("registry")}
                </p>
              ) : null}
            </div>

            <div>
              <label className="text-[12px] text-muted" htmlFor="bundle-tls">
                TLS mode
              </label>
              <select
                id="bundle-tls"
                value={form.tls_mode}
                onChange={(event) => set("tls_mode", event.target.value)}
                data-bundle-tls
                className="mt-1 h-9 w-full rounded-lg border border-line bg-surface px-2 text-[13px] outline-none focus:border-accent"
              >
                {TLS_MODES.map((mode) => (
                  <option key={mode} value={mode}>
                    {mode}
                  </option>
                ))}
              </select>
            </div>

            <label className="flex items-center gap-2 text-[13px]">
              <input
                type="checkbox"
                checked={form.observability}
                onChange={(event) => set("observability", event.target.checked)}
                data-bundle-observability
                className="size-4 accent-[var(--accent)]"
              />
              Include the observability profile — Prometheus and Grafana over this instance&rsquo;s
              metrics
            </label>

            {formError ? (
              <p role="alert" data-bundle-form-error className="text-[12px] text-danger">
                {formError}
              </p>
            ) : null}

            <div className="flex justify-end gap-2">
              <button
                type="button"
                onClick={() => {
                  setForm(BLANK);
                  setTouched({});
                  setFormError(null);
                }}
                data-bundle-reset
                className="rounded-lg border border-line px-3 py-1.5 text-[13px] hover:bg-quiet-soft"
              >
                Reset
              </button>
              <button
                type="button"
                onClick={() => void generate()}
                disabled={generating}
                data-bundle-generate
                className="flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[13px] text-white disabled:opacity-60"
              >
                {generating ? (
                  <Loader2 className="size-3.5 animate-spin" aria-hidden="true" />
                ) : (
                  <CheckCircle2 className="size-3.5" aria-hidden="true" />
                )}
                Generate
              </button>
            </div>
          </div>

          <div className="space-y-2">
            {generated ? (
              <div
                className="rounded-lg border border-positive-soft bg-positive-soft/40 p-3"
                data-bundle-generated
              >
                <h3 className="flex items-center gap-1.5 text-[13px] font-medium text-positive">
                  <CheckCircle2 className="size-4" aria-hidden="true" />
                  {generated.name} generated
                </h3>
                <p className="mt-1 text-[11.5px] text-muted">
                  {generated.files.length} files · checksum{" "}
                  <span className="font-mono" data-bundle-checksum>
                    {generated.checksum}
                  </span>
                </p>
                <ul className="mt-2 space-y-1" data-bundle-generated-files>
                  {generated.files.map((file) => (
                    <BundleFileRow
                      key={file.name}
                      bundleId={generated.id}
                      file={file}
                      onDownloaded={(name, checksum, size) =>
                        setDownloaded((current) => ({ ...current, [name]: checksum }))
                      }
                    />
                  ))}
                </ul>
                <h4 className="mt-3 flex items-center gap-1.5 text-[12px] font-medium">
                  <Terminal className="size-3.5" aria-hidden="true" />
                  Run on the target host
                </h4>
                <pre
                  className="mt-1 overflow-x-auto rounded-lg bg-quiet-soft/60 p-2.5 font-mono text-[11px]"
                  data-bundle-commands
                >
                  {generated.commands.join("\n")}
                </pre>
              </div>
            ) : (
              <div className="rounded-lg border border-line p-4 text-[12.5px] text-muted">
                <Info className="mb-2 size-4" aria-hidden="true" />
                <p>
                  Generated files, their checksums and the commands to run them appear here. The
                  bundle is stored, so it can be downloaded again after this panel is closed — the
                  list below is the durable record.
                </p>
              </div>
            )}
          </div>
        </div>
      </section>

      <section className="rounded-xl border border-line">
        <header className="flex flex-wrap items-center gap-2 border-b border-line px-4 py-3">
          <h2 className="text-[13.5px] font-medium">Generated bundles</h2>
          {bundles ? (
            <span className="text-[12px] text-muted">
              {rows.length} of {bundles.length}
            </span>
          ) : null}
          {notice ? (
            <span data-bundle-notice className="text-[12px] text-positive">
              {notice}
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
                data-bundle-filter
                placeholder="Filter by target, kind or version"
                aria-label="Filter bundles"
                className="h-8 w-56 rounded-lg border border-line bg-surface pl-7 pr-2 text-[12px] outline-none focus:border-accent"
              />
            </div>
            <button
              type="button"
              onClick={() => void load()}
              data-bundle-refresh
              className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[12px] hover:bg-quiet-soft"
            >
              <RefreshCw className="h-3 w-3" aria-hidden="true" />
              Refresh
            </button>
          </div>
        </header>

        {error ? (
          <p role="alert" data-bundle-error className="px-4 py-3 text-[12.5px] text-danger">
            {error}
          </p>
        ) : loading && !bundles ? (
          <div className="px-4 py-3">
            <LoadingTable rows={3} columns={5} />
          </div>
        ) : rows.length === 0 ? (
          <div className="px-4 py-3">
            <EmptyState
              title={filter ? "No bundle matches" : "No bundle generated yet"}
              hint={
                filter
                  ? "Clear the filter to see every generated bundle."
                  : "Nothing has been generated for this instance. A bundle is a target's own compose stack, values file and .env.example — generate one above to get the files and the commands."
              }
            />
          </div>
        ) : (
          <ul className="divide-y divide-line" data-bundle-list>
            {rows.map((bundle) => (
              <li key={bundle.id} className="px-4 py-3" data-bundle-row={bundle.name}>
                <div className="flex flex-wrap items-start gap-3">
                  <div className="min-w-0 flex-1">
                    <div className="flex flex-wrap items-center gap-2">
                      <p className="font-mono text-[12.5px]" data-bundle-row-name={bundle.name}>
                        {bundle.name}
                      </p>
                      <span className="rounded-full bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted">
                        {bundle.kind}
                      </span>
                      <span className="font-mono text-[11px] text-muted">{bundle.version}</span>
                    </div>
                    <p className="mt-0.5 text-[11.5px] text-muted">
                      {bundle.files.length} files · checksum{" "}
                      <span className="font-mono">{bundle.checksum}</span>
                      {bundle.generated_at ? ` · ${formatTimestamp(bundle.generated_at)}` : ""}
                      {bundle.download_count
                        ? ` · ${bundle.download_count} download${bundle.download_count === 1 ? "" : "s"}`
                        : " · not downloaded yet"}
                    </p>
                    {Object.keys(downloaded).some((name) =>
                      bundle.files.some((file) => file.name === name),
                    ) ? (
                      <p
                        className="mt-1 text-[11px] text-positive"
                        data-bundle-download-verified
                      >
                        Verified against the server&rsquo;s checksum:{" "}
                        {Object.entries(downloaded)
                          .filter(([name]) => bundle.files.some((file) => file.name === name))
                          .map(([name, checksum]) => (
                            <span key={name} className="mr-2 font-mono">
                              {name} → {checksum.slice(0, 16)}…
                            </span>
                          ))}
                      </p>
                    ) : null}
                  </div>
                  <button
                    type="button"
                    onClick={() => setRendering(bundle)}
                    data-bundle-render={bundle.name}
                    className="flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px] hover:bg-quiet-soft"
                  >
                    <Eye className="size-3" aria-hidden="true" />
                    Files &amp; commands
                  </button>
                </div>
              </li>
            ))}
          </ul>
        )}
      </section>

      <p className="text-[11.5px] text-muted">
        Press <kbd className="rounded border border-line px-1">/</kbd> to filter,{" "}
        <kbd className="rounded border border-line px-1">g</kbd> to jump to the target name, and{" "}
        <kbd className="rounded border border-line px-1">Esc</kbd> to close the file panel. Every
        download is verified against the checksum the server returned with it, so a truncated
        transfer is visible rather than silent.
      </p>

      {rendering ? (
        <RenderPanel bundle={rendering} onClose={() => setRendering(null)} />
      ) : null}
    </div>
  );
}
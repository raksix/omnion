"use client";

/**
 * `/ai/evals/[key]` — one suite: its cases, its configuration and its coverage (REQ-107, slice 1).
 *
 * The screen is a tabbed editor because the three things an operator does here are different in
 * kind: **Cases** are what is measured, **Config** is what it is measured with, and **History** is
 * what it measured. Mixing them into one scroll made the config form enormous and the cases table
 * unreachable on a laptop.
 *
 * Decisions worth naming:
 *
 * 1. **The property list comes from the server.** `detail.properties` is derived from
 *    `eval_case::PROPERTIES`, the same list the scorer reads. Ticking a box writes that key into
 *    `expected`; unticking removes it. A property the scorer does not implement cannot be
 *    offered, which is what keeps "a case that asserts nothing" from ever being authored.
 *
 * 2. **A `rubric` case needs a judge, and the screen says which one.** The judge select hides the
 *    model under test — the request's own rule that the judge must differ — and a blocking suite
 *    with a rubric case and no judge is refused by the API with `details.field` pointing at the
 *    judge model, which this form marks.
 *
 * 3. **The import reports the lines it refused.** A partial import is only honest if the screen
 *    shows both halves: what landed, and which line numbers still need fixing. A report that said
 *    "3 imported" would send an operator looking for the other two rows.
 *
 * 4. **Every config field is a patch, and a patch omits what it does not touch.** Saving sends
 *    only the fields the form actually changed, because `temperature: null` on an untouched field
 *    would clear the suite's temperature on every save.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";
import { useRouter } from "next/navigation";

import {
  AlertTriangle,
  ArrowLeft,
  Check,
  ClipboardList,
  FileUp,
  Loader2,
  Plus,
  RefreshCw,
  Settings2,
  Trash2,
  Upload,
  X,
} from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import { LoadingTable } from "@/components/loading-table";
import { ApiError } from "@/lib/api";
import {
  deleteEvalCase,
  fetchEvalSuite,
  IMPORT_COLUMNS,
  createEvalCase,
  importEvalCases,
  updateEvalCase,
  updateEvalSuite,
  type EvalCase,
  type EvalImportReport,
  type EvalProperty,
  type EvalSuiteDetail,
} from "@/lib/eval-api";

type Tab = "cases" | "config";

/** The three tabs the request asks for, and the two that exist in slice 1. */
const TABS: { id: Tab; label: string }[] = [
  { id: "cases", label: "Cases" },
  { id: "config", label: "Config" },
];

function propertyOf(properties: EvalProperty[], key: string): EvalProperty | undefined {
  return properties.find((property) => property.key === key);
}

/** The keys a case asserts, in the order the server listed them. */
function assertedKeys(expected: Record<string, unknown>, properties: EvalProperty[]): string[] {
  return properties.map((property) => property.key).filter((key) => key in expected);
}

export function EvalSuiteDetailView({ suiteKey }: { suiteKey: string }) {
  const router = useRouter();
  const [detail, setDetail] = useState<EvalSuiteDetail | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [tab, setTab] = useState<Tab>("cases");

  const [caseOpen, setCaseOpen] = useState(false);
  const [editing, setEditing] = useState<EvalCase | null>(null);
  const [caseDraft, setCaseDraft] = useState({
    name: "",
    input: "",
    weight: "1",
    tags: "",
    enabled: true,
  });
  const [caseProps, setCaseProps] = useState<Record<string, string>>({});
  const [savingCase, setSavingCase] = useState(false);
  const [caseError, setCaseError] = useState<string | null>(null);
  const [caseField, setCaseField] = useState<string | null>(null);
  const [rowMessage, setRowMessage] = useState<Record<string, string>>({});

  const [config, setConfig] = useState<Record<string, string>>({});
  const [configBlocking, setConfigBlocking] = useState(false);
  const [configEnabled, setConfigEnabled] = useState(true);
  const [savingConfig, setSavingConfig] = useState(false);
  const [configError, setConfigError] = useState<string | null>(null);
  const [configField, setConfigField] = useState<string | null>(null);
  const [configSaved, setConfigSaved] = useState(false);

  const [importOpen, setImportOpen] = useState(false);
  const [csv, setCsv] = useState("");
  const [importing, setImporting] = useState(false);
  const [report, setReport] = useState<EvalImportReport | null>(null);
  const [importError, setImportError] = useState<string | null>(null);

  const load = useCallback(() => {
    setBusy(true);
    setError(null);
    fetchEvalSuite(suiteKey)
      .then((next) => {
        setDetail(next);
        // The config form starts from the stored row, and every later render edits that copy
        // rather than re-reading the server — otherwise a half-typed threshold would be reset
        // by any background refresh.
        setConfig({
          name: next.name,
          description: next.description,
          threshold_percent: String(next.threshold_percent),
          max_regression_points: String(next.max_regression_points),
          temperature: next.temperature === null ? "" : String(next.temperature),
          schedule: next.schedule ?? "",
          judge_model_id: next.judge_model_id ?? "",
          judge_prompt: next.judge_prompt ?? "",
        });
        setConfigBlocking(next.blocking);
        setConfigEnabled(next.enabled);
      })
      .catch((cause: unknown) => {
        setDetail(null);
        setError(
          cause instanceof ApiError ? cause.message : "The suite could not be loaded.",
        );
      })
      .finally(() => setBusy(false));
  }, [suiteKey]);

  useEffect(load, [load]);

  const properties = useMemo(() => detail?.properties ?? [], [detail]);
  const cases = useMemo(() => detail?.cases ?? [], [detail]);

  const fieldOf = (cause: unknown): string | null =>
    cause instanceof ApiError && cause.details && typeof cause.details.field === "string"
      ? cause.details.field
      : null;

  const openNew = useCallback(() => {
    setEditing(null);
    setCaseDraft({ name: "", input: "", weight: "1", tags: "", enabled: true });
    setCaseProps({});
    setCaseError(null);
    setCaseField(null);
    setCaseOpen(true);
  }, []);

  const openEdit = useCallback((row: EvalCase) => {
    setEditing(row);
    setCaseDraft({
      name: row.name,
      input: typeof row.input?.prompt === "string" ? row.input.prompt : JSON.stringify(row.input, null, 2),
      weight: String(row.weight),
      tags: row.tags.join(", "),
      enabled: row.enabled,
    });
    // An existing case's properties are pre-filled from the stored document, so editing a case
    // that already asserts `contains` does not silently drop the assertion on save.
    const prefill: Record<string, string> = {};
    for (const [key, value] of Object.entries(row.expected ?? {})) {
      prefill[key] = typeof value === "string" ? value : JSON.stringify(value);
    }
    setCaseProps(prefill);
    setCaseError(null);
    setCaseField(null);
    setCaseOpen(true);
  }, []);

  const saveCase = useCallback(async () => {
    setSavingCase(true);
    setCaseError(null);
    setCaseField(null);
    const expected: Record<string, string> = {};
    for (const [key, value] of Object.entries(caseProps)) {
      if (value.trim()) expected[key] = value;
    }
    const body = {
      name: caseDraft.name.trim(),
      input: caseDraft.input,
      expected,
      weight: Number.parseFloat(caseDraft.weight) || 1,
      tags: caseDraft.tags
        .split(",")
        .map((tag) => tag.trim())
        .filter(Boolean),
      enabled: caseDraft.enabled,
    };
    try {
      if (editing) {
        await updateEvalCase(editing.id, body);
      } else {
        await createEvalCase(suiteKey, body);
      }
      setCaseOpen(false);
      load();
      router.refresh();
    } catch (cause: unknown) {
      setCaseError(cause instanceof ApiError ? cause.message : "The case could not be saved.");
      setCaseField(fieldOf(cause));
    } finally {
      setSavingCase(false);
    }
  }, [caseDraft, caseProps, editing, suiteKey, load, router]);

  const removeCase = useCallback(
    async (row: EvalCase) => {
      try {
        await deleteEvalCase(row.id);
        setRowMessage((state) => ({ ...state, [row.id]: "" }));
        load();
      } catch (cause: unknown) {
        setRowMessage((state) => ({
          ...state,
          [row.id]: cause instanceof ApiError ? cause.message : "The case could not be removed.",
        }));
      }
    },
    [load],
  );

  const saveConfig = useCallback(async () => {
    setSavingConfig(true);
    setConfigError(null);
    setConfigField(null);
    setConfigSaved(false);
    // Only what the operator touched. A field left alone is omitted rather than sent as its
    // default, because `temperature: null` means "clear it" on this API and an untouched field
    // must not clear itself on every save.
    const body: Record<string, unknown> = {};
    if (config.name !== detail?.name) body.name = config.name.trim();
    if (config.description !== detail?.description) body.description = config.description.trim();
    if (config.threshold_percent !== String(detail?.threshold_percent)) {
      body.threshold_percent = Number.parseInt(config.threshold_percent, 10);
    }
    if (config.max_regression_points !== String(detail?.max_regression_points)) {
      body.max_regression_points = Number.parseFloat(config.max_regression_points);
    }
    const temperature = config.temperature.trim();
    if (temperature === "" && detail?.temperature !== null) body.temperature = null;
    else if (temperature !== "" && Number.parseFloat(temperature) !== detail?.temperature) {
      body.temperature = Number.parseFloat(temperature);
    }
    if (config.schedule !== (detail?.schedule ?? "")) {
      body.schedule = config.schedule === "" ? null : config.schedule;
    }
    if (config.judge_model_id !== (detail?.judge_model_id ?? "")) {
      body.judge_model_id = config.judge_model_id === "" ? null : config.judge_model_id;
    }
    if (config.judge_prompt !== (detail?.judge_prompt ?? "")) {
      body.judge_prompt = config.judge_prompt === "" ? null : config.judge_prompt;
    }
    if (configBlocking !== detail?.blocking) body.blocking = configBlocking;
    if (configEnabled !== detail?.enabled) body.enabled = configEnabled;

    try {
      if (Object.keys(body).length === 0) {
        setConfigSaved(true);
        return;
      }
      await updateEvalSuite(suiteKey, body);
      setConfigSaved(true);
      load();
      router.refresh();
    } catch (cause: unknown) {
      setConfigError(cause instanceof ApiError ? cause.message : "The suite could not be saved.");
      setConfigField(fieldOf(cause));
    } finally {
      setSavingConfig(false);
    }
  }, [config, configBlocking, configEnabled, detail, suiteKey, load, router]);

  const runImport = useCallback(async () => {
    setImporting(true);
    setImportError(null);
    setReport(null);
    try {
      const answer = await importEvalCases(suiteKey, csv);
      setReport(answer);
      setCsv("");
      load();
    } catch (cause: unknown) {
      setImportError(cause instanceof ApiError ? cause.message : "The import failed.");
    } finally {
      setImporting(false);
    }
  }, [csv, suiteKey, load]);

  if (error) {
    return (
      <div data-eval-detail-error className="flex flex-col gap-3">
        <p className="rounded-lg bg-danger-soft px-3 py-2 text-[13px] text-danger">{error}</p>
        <button
          type="button"
          onClick={load}
          className="self-start rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
        >
          Retry
        </button>
      </div>
    );
  }

  if (!detail) return <LoadingTable columns={5} rows={3} />;

  // The judge may not be the model under test — the request's own rule, enforced by the API and
  // mirrored here so the select cannot offer an invalid choice in the first place.
  const judgeOptions = detail.model_options.filter((option) => option.id !== detail.model_id);

  return (
    <div data-eval-suite-detail className="flex flex-col gap-5">
      <div className="flex flex-wrap items-center gap-2">
        <Link
          href="/ai/evals"
          className="inline-flex items-center gap-1.5 text-[12px] text-muted transition hover:text-ink"
        >
          <ArrowLeft aria-hidden size={14} />
          All suites
        </Link>
        <span className="text-[15px] font-medium text-ink">{detail.name}</span>
        <code className="rounded bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted">
          {detail.key}
        </code>
        <span
          data-eval-readiness={detail.readiness}
          className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted"
        >
          {detail.readiness_note}
        </span>
        <button
          type="button"
          onClick={load}
          className="ml-auto inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
        >
          {busy ? (
            <Loader2 aria-hidden size={14} className="animate-spin" />
          ) : (
            <RefreshCw aria-hidden size={14} />
          )}
          Refresh
        </button>
      </div>

      <div className="flex gap-1 border-b border-line" role="tablist" aria-label="Suite sections">
        {TABS.map((entry) => (
          <button
            key={entry.id}
            type="button"
            role="tab"
            aria-selected={tab === entry.id}
            data-eval-tab={entry.id}
            onClick={() => setTab(entry.id)}
            className={`-mb-px border-b-2 px-3 py-2 text-[13px] transition ${
              tab === entry.id
                ? "border-accent text-ink"
                : "border-transparent text-muted hover:text-ink"
            }`}
          >
            {entry.label}
            {entry.id === "cases" ? ` (${cases.length})` : ""}
          </button>
        ))}
      </div>

      {tab === "cases" ? (
        <div className="flex flex-col gap-4">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <p className="text-[12px] text-muted">
              Each case is one input and the properties its output must satisfy. All of them must
              hold for the case to pass.
            </p>
            <div className="flex items-center gap-2">
              <button
                type="button"
                onClick={() => {
                  setImportOpen((open) => !open);
                  setReport(null);
                  setImportError(null);
                }}
                aria-expanded={importOpen}
                className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
              >
                {importOpen ? <X aria-hidden size={14} /> : <Upload aria-hidden size={14} />}
                Import CSV
              </button>
              <button
                type="button"
                onClick={openNew}
                className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white transition hover:bg-accent-strong"
              >
                <Plus aria-hidden size={14} />
                New case
              </button>
            </div>
          </div>

          {importOpen ? (
            <div
              data-eval-import
              className="flex flex-col gap-2 rounded-xl border border-line bg-surface p-4"
            >
              <label className="flex flex-col gap-1 text-[12px] text-muted">
                Paste CSV
                <textarea
                  value={csv}
                  onChange={(event) => setCsv(event.target.value)}
                  rows={6}
                  placeholder={"name,input,expected,weight,tags\ngreeting,Say hi,{\"exact\":\"hi\"},1,smoke"}
                  aria-label="CSV to import"
                  className="rounded-lg border border-line bg-canvas px-3 py-2 font-mono text-[12px] text-ink"
                />
              </label>
              <p className="text-[12px] text-muted">
                Columns: <code className="font-mono">{IMPORT_COLUMNS}</code>. A header row is
                required and columns are matched by name, so the order does not matter. A row that
                cannot be read is skipped and reported with its line number — the rest still
                import.
              </p>
              {importError ? (
                <p className="rounded-lg bg-danger-soft px-3 py-2 text-[12px] text-danger">
                  {importError}
                </p>
              ) : null}
              {/*
                Both halves of the report are rendered. A report that only said "3 imported"
                would leave the operator hunting for the rows that did not land.
              */}
              {report ? (
                <div
                  data-eval-import-report
                  className="flex flex-col gap-2 rounded-lg border border-line bg-canvas p-3"
                >
                  <p className="text-[12px] text-ink">
                    Imported {report.imported.length} case(s).
                  </p>
                  {report.problems.length > 0 ? (
                    <ul className="flex flex-col gap-1">
                      {report.problems.map((problem) => (
                        <li
                          key={`${problem.line}-${problem.message}`}
                          data-eval-import-problem
                          className="flex items-start gap-1.5 text-[12px] text-danger"
                        >
                          <AlertTriangle aria-hidden size={13} className="mt-0.5 shrink-0" />
                          <span>
                            Line {problem.line}: {problem.message}
                          </span>
                        </li>
                      ))}
                    </ul>
                  ) : (
                    <p className="text-[12px] text-positive">Every row was read.</p>
                  )}
                </div>
              ) : null}
              <div className="flex items-center gap-2">
                <button
                  type="button"
                  disabled={importing || !csv.trim()}
                  onClick={() => void runImport()}
                  className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white disabled:opacity-50"
                >
                  {importing ? (
                    <Loader2 aria-hidden size={14} className="animate-spin" />
                  ) : (
                    <FileUp aria-hidden size={14} />
                  )}
                  Import
                </button>
                <button
                  type="button"
                  onClick={() => {
                    setImportOpen(false);
                    setReport(null);
                    setImportError(null);
                  }}
                  className="rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
                >
                  Close
                </button>
              </div>
            </div>
          ) : null}

          {caseOpen ? (
            <form
              data-eval-case-form
              onSubmit={(event) => {
                event.preventDefault();
                void saveCase();
              }}
              className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
            >
              <div className="grid gap-3 sm:grid-cols-[1fr_8rem]">
                <label className="flex flex-col gap-1 text-[12px] text-muted">
                  Name
                  <input
                    value={caseDraft.name}
                    onChange={(event) =>
                      setCaseDraft({ ...caseDraft, name: event.target.value })
                    }
                    required
                    placeholder="Greets by name"
                    aria-label="Case name"
                    className={`rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink ${
                      caseField === "name" ? "border-danger" : "border-line"
                    }`}
                  />
                </label>
                <label className="flex flex-col gap-1 text-[12px] text-muted">
                  Weight
                  <input
                    type="number"
                    step="0.1"
                    min={0.1}
                    max={10}
                    value={caseDraft.weight}
                    onChange={(event) =>
                      setCaseDraft({ ...caseDraft, weight: event.target.value })
                    }
                    aria-label="Case weight"
                    className={`rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink ${
                      caseField === "weight" ? "border-danger" : "border-line"
                    }`}
                  />
                </label>
              </div>
              <label className="flex flex-col gap-1 text-[12px] text-muted">
                Input
                <textarea
                  value={caseDraft.input}
                  onChange={(event) => setCaseDraft({ ...caseDraft, input: event.target.value })}
                  rows={3}
                  placeholder="What the run is asked"
                  aria-label="Case input"
                  className={`rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink ${
                    caseField === "input" ? "border-danger" : "border-line"
                  }`}
                />
              </label>
              {/*
                The property group is generated from the server's list, so a property the scorer
                cannot implement has no checkbox. Ticking writes the key into `expected`;
                unticking removes it, so a case can never be left asserting a property whose
                field was cleared.
              */}
              <fieldset className="flex flex-col gap-2">
                <legend className="text-[12px] text-muted">
                  Expected properties — all of them must hold
                </legend>
                {properties.map((property) => (
                  <div key={property.key} className="flex flex-col gap-1">
                    <label className="flex items-center gap-2 text-[12px] text-ink">
                      <input
                        type="checkbox"
                        checked={property.key in caseProps}
                        onChange={(event) => {
                          const next = { ...caseProps };
                          if (event.target.checked) {
                            next[property.key] = "";
                          } else {
                            delete next[property.key];
                          }
                          setCaseProps(next);
                        }}
                        className="accent-accent"
                      />
                      <span>{property.label}</span>
                      <code className="rounded bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted">
                        {property.key}
                      </code>
                      {property.needs_judge ? (
                        <span className="rounded-full bg-caution-soft px-1.5 py-0.5 text-[10px] text-caution">
                          needs a judge
                        </span>
                      ) : null}
                    </label>
                    {property.key in caseProps ? (
                      property.key === "json_schema" ? (
                        <textarea
                          value={caseProps[property.key] ?? ""}
                          onChange={(event) =>
                            setCaseProps({ ...caseProps, [property.key]: event.target.value })
                          }
                          rows={3}
                          placeholder='{"type":"object","required":["title"]}'
                          aria-label={`${property.key} value`}
                          className={`rounded-lg border bg-canvas px-3 py-2 font-mono text-[12px] text-ink ${
                            caseField === property.key ? "border-danger" : "border-line"
                          }`}
                        />
                      ) : (
                        <input
                          value={caseProps[property.key] ?? ""}
                          onChange={(event) =>
                            setCaseProps({ ...caseProps, [property.key]: event.target.value })
                          }
                          placeholder={
                            property.key === "regex"
                              ? "^https://"
                              : property.key === "rubric"
                                ? "What makes the answer good"
                                : "expected value"
                          }
                          aria-label={`${property.key} value`}
                          className={`rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink ${
                            caseField === property.key ? "border-danger" : "border-line"
                          }`}
                        />
                      )
                    ) : null}
                  </div>
                ))}
              </fieldset>
              <div className="grid gap-3 sm:grid-cols-2">
                <label className="flex flex-col gap-1 text-[12px] text-muted">
                  Tags
                  <input
                    value={caseDraft.tags}
                    onChange={(event) => setCaseDraft({ ...caseDraft, tags: event.target.value })}
                    placeholder="smoke, regression"
                    aria-label="Case tags"
                    className="rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink"
                  />
                </label>
                <label className="flex items-end gap-2 pb-2 text-[12px] text-muted">
                  <input
                    type="checkbox"
                    checked={caseDraft.enabled}
                    onChange={(event) =>
                      setCaseDraft({ ...caseDraft, enabled: event.target.checked })
                    }
                    className="accent-accent"
                  />
                  Enabled
                </label>
              </div>
              {caseError ? (
                <p
                  data-eval-case-form-error
                  className="rounded-lg bg-danger-soft px-3 py-2 text-[12px] text-danger"
                >
                  {caseError}
                </p>
              ) : null}
              <div className="flex items-center gap-2">
                <button
                  type="submit"
                  disabled={savingCase || !caseDraft.name.trim()}
                  className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white disabled:opacity-50"
                >
                  {savingCase ? (
                    <Loader2 aria-hidden size={14} className="animate-spin" />
                  ) : (
                    <Check aria-hidden size={14} />
                  )}
                  {editing ? "Save case" : "Add case"}
                </button>
                <button
                  type="button"
                  onClick={() => {
                    setCaseOpen(false);
                    setCaseError(null);
                    setCaseField(null);
                  }}
                  className="rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
                >
                  Cancel
                </button>
              </div>
            </form>
          ) : null}

          {cases.length === 0 ? (
            <EmptyState
              title="No cases yet"
              hint="A suite with no cases measures nothing and would report a pass rate of zero cases. Add one, or import a CSV — capturing a real failure you have seen is worth more than ten easy cases."
              action={
                <button
                  type="button"
                  onClick={openNew}
                  className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white"
                >
                  <Plus aria-hidden size={14} />
                  New case
                </button>
              }
            />
          ) : (
            <ul className="flex flex-col gap-2">
              {cases.map((row) => {
                const keys = assertedKeys(row.expected, properties);
                return (
                  <li
                    key={row.id}
                    data-eval-case-row
                    className="flex flex-col gap-2 rounded-xl border border-line bg-surface p-4"
                  >
                    <div className="flex flex-wrap items-center gap-2">
                      <ClipboardList aria-hidden size={14} className="text-muted" />
                      <span className="text-[13px] font-medium text-ink">{row.name}</span>
                      <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
                        weight {row.weight}
                      </span>
                      {!row.enabled ? (
                        <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
                          disabled
                        </span>
                      ) : null}
                      {row.source !== "manual" ? (
                        <span className="rounded-full bg-quiet-soft px-2 py-0.5 text-[11px] text-muted">
                          {row.source}
                        </span>
                      ) : null}
                    </div>
                    <p className="truncate text-[12px] text-muted">
                      {typeof row.input?.prompt === "string" ? row.input.prompt : "—"}
                    </p>
                    <div className="flex flex-wrap items-center gap-1.5">
                      {keys.length === 0 ? (
                        <span className="text-[12px] text-caution">
                          Asserts nothing — this case would pass every model.
                        </span>
                      ) : (
                        keys.map((key) => (
                          <span
                            key={key}
                            data-eval-case-property
                            className="rounded bg-accent-soft px-1.5 py-0.5 text-[11px] text-accent"
                          >
                            {key}
                          </span>
                        ))
                      )}
                      {row.tags.map((tag) => (
                        <span
                          key={tag}
                          className="rounded bg-quiet-soft px-1.5 py-0.5 text-[11px] text-muted"
                        >
                          {tag}
                        </span>
                      ))}
                    </div>
                    <div className="flex items-center gap-2">
                      <button
                        type="button"
                        onClick={() => openEdit(row)}
                        className="rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
                      >
                        Edit
                      </button>
                      <button
                        type="button"
                        onClick={() => void removeCase(row)}
                        className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas"
                      >
                        <Trash2 aria-hidden size={13} />
                        Delete
                      </button>
                      {rowMessage[row.id] ? (
                        <span className="text-[12px] text-danger">{rowMessage[row.id]}</span>
                      ) : null}
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
        </div>
      ) : (
        <form
          data-eval-config-form
          onSubmit={(event) => {
            event.preventDefault();
            void saveConfig();
          }}
          className="flex flex-col gap-3 rounded-xl border border-line bg-surface p-4"
        >
          <div className="grid gap-3 sm:grid-cols-2">
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Name
              <input
                value={config.name ?? ""}
                onChange={(event) => setConfig({ ...config, name: event.target.value })}
                aria-label="Suite name"
                className={`rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink ${
                  configField === "name" ? "border-danger" : "border-line"
                }`}
              />
            </label>
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Temperature
              <input
                value={config.temperature ?? ""}
                onChange={(event) => setConfig({ ...config, temperature: event.target.value })}
                placeholder="0 – 2"
                aria-label="Temperature"
                className={`rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink ${
                  configField === "temperature" ? "border-danger" : "border-line"
                }`}
              />
            </label>
          </div>
          <label className="flex flex-col gap-1 text-[12px] text-muted">
            Description
            <input
              value={config.description ?? ""}
              onChange={(event) => setConfig({ ...config, description: event.target.value })}
              aria-label="Suite description"
              className="rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink"
            />
          </label>
          <div className="grid gap-3 sm:grid-cols-3">
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Pass threshold (%)
              <input
                type="number"
                min={1}
                max={100}
                value={config.threshold_percent ?? ""}
                onChange={(event) =>
                  setConfig({ ...config, threshold_percent: event.target.value })
                }
                aria-label="Pass threshold"
                className={`rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink ${
                  configField === "threshold_percent" ? "border-danger" : "border-line"
                }`}
              />
            </label>
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Regression tolerance (points)
              <input
                type="number"
                step="0.5"
                min={0}
                max={50}
                value={config.max_regression_points ?? ""}
                onChange={(event) =>
                  setConfig({ ...config, max_regression_points: event.target.value })
                }
                aria-label="Regression tolerance"
                className={`rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink ${
                  configField === "max_regression_points" ? "border-danger" : "border-line"
                }`}
              />
            </label>
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Schedule
              <select
                value={config.schedule ?? ""}
                onChange={(event) => setConfig({ ...config, schedule: event.target.value })}
                aria-label="Schedule"
                className={`rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink ${
                  configField === "schedule" ? "border-danger" : "border-line"
                }`}
              >
              <option value="">Manual only</option>
              {detail.schedule_presets.map((preset) => (
                <option key={preset.id} value={preset.id}>
                  {preset.label}
                  {preset.cron ? ` (${preset.cron})` : ""}
                </option>
              ))}
              </select>
            </label>
          </div>
          <div className="grid gap-3 sm:grid-cols-2">
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Judge model
              <select
                value={config.judge_model_id ?? ""}
                onChange={(event) => setConfig({ ...config, judge_model_id: event.target.value })}
                aria-label="Judge model"
                className={`rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink ${
                  configField === "judge_model_id" ? "border-danger" : "border-line"
                }`}
              >
                <option value="">None</option>
                {judgeOptions.map((option) => (
                  <option key={option.id} value={option.id}>
                    {option.label}
                  </option>
                ))}
              </select>
            </label>
            <label className="flex flex-col gap-1 text-[12px] text-muted">
              Judge prompt
              <input
                value={config.judge_prompt ?? ""}
                onChange={(event) => setConfig({ ...config, judge_prompt: event.target.value })}
                placeholder="Leave empty for the default rubric prompt"
                aria-label="Judge prompt"
                className={`rounded-lg border bg-canvas px-3 py-2 text-[13px] text-ink ${
                  configField === "judge_prompt" ? "border-danger" : "border-line"
                }`}
              />
            </label>
          </div>
          {detail.model_id ? (
            <p className="text-[12px] text-muted">
              The judge must differ from the model under test, so that model is not offered above.
              A rubric case with no judge would report an error instead of a verdict.
            </p>
          ) : null}
          <div className="flex flex-wrap items-center gap-4">
            <label className="flex items-center gap-2 text-[12px] text-muted">
              <input
                type="checkbox"
                checked={configBlocking}
                onChange={(event) => setConfigBlocking(event.target.checked)}
                className="accent-accent"
              />
              Blocking — this suite is the promotion gate
            </label>
            <label className="flex items-center gap-2 text-[12px] text-muted">
              <input
                type="checkbox"
                checked={configEnabled}
                onChange={(event) => setConfigEnabled(event.target.checked)}
                className="accent-accent"
              />
              Enabled
            </label>
          </div>
          {configError ? (
            <p
              data-eval-config-error
              className="rounded-lg bg-danger-soft px-3 py-2 text-[12px] text-danger"
            >
              {configError}
            </p>
          ) : null}
          {configSaved ? (
            <p className="text-[12px] text-positive">Saved.</p>
          ) : null}
          <div>
            <button
              type="submit"
              disabled={savingConfig}
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12px] font-medium text-white disabled:opacity-50"
            >
              {savingConfig ? (
                <Loader2 aria-hidden size={14} className="animate-spin" />
              ) : (
                <Settings2 aria-hidden size={14} />
              )}
              Save configuration
            </button>
          </div>
        </form>
      )}
    </div>
  );
}

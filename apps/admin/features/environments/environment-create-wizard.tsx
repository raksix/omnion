"use client";

/**
 * The create wizard (REQ-017, slice 2).
 *
 * Four steps, because that is how an operator actually decides: name it, choose where it is
 * served, choose what it carries, confirm. Two rules earn their place:
 *
 * - **The area list comes from the API, never from a local constant.** The panel hard-codes its
 *   own idea of what a clone can copy and the server refuses names it does not know; the two
 *   lists drift, and the drift shows up as a `400` on submit. The options, their labels and what
 *   they cost are all read from `GET /environments`.
 * - **"At least one area" is checked before the request, with the message the API would have
 *   given.** A wizard that submits an empty array to learn "Choose at least one area" has already
 *   made the operator read a network error to learn something the screen could have said.
 *
 * The response comes back with the environment in `cloning` state — the copy runs in the API's
 * worker, so the wizard closes the moment the row exists and the list screen polls from there.
 */
import { useEffect, useMemo, useRef, useState } from "react";

import { Check, X } from "lucide-react";

import { ApiError, createEnvironment } from "@/lib/api";
import type { Environment, EnvironmentAreaOption } from "@/lib/types";

type Props = {
  /** The areas the API offers, with labels and costs. */
  areas: EnvironmentAreaOption[];
  /** The production environment the clone reads from. */
  sourceKey: string;
  onClose: () => void;
  onCreated: (environment: Environment) => void;
};

/** The steps, in order. The wizard is a stepper rather than one long form on purpose. */
const STEPS = ["Name", "Host", "Content", "Confirm"] as const;

export function EnvironmentCreateWizard({ areas, sourceKey, onClose, onCreated }: Props) {
  const [step, setStep] = useState(0);
  const [name, setName] = useState("");
  const [key, setKey] = useState("");
  const [host, setHost] = useState("");
  const [selected, setSelected] = useState<string[]>([]);
  const [excludeArchived, setExcludeArchived] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const nameRef = useRef<HTMLInputElement>(null);
  const dialogRef = useRef<HTMLDivElement>(null);

  // Default to every area the API offers **that copies**. A clone that copies nothing is not a
  // staging environment, and choosing a subset is the deliberate act, so the working areas are
  // the default. The three that copy nothing are deliberately not in that default: they were
  // ticked by default before, stored on the job, priced in the estimate and then produced zero
  // rows, so an operator who never looked closely believed a settings copy had happened.
  //
  // They stay on screen, unticked and explained. Hiding them would be the request's forbidden
  // "hidden feature" and would cost the operator the knowledge that staging shares those rows.
  useEffect(() => {
    setSelected(areas.filter((area) => area.copies).map((area) => area.name));
  }, [areas]);

  // Escape closes, and the first step takes focus so the wizard is usable from the keyboard
  // without hunting for the field.
  useEffect(() => {
    nameRef.current?.focus();
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const nameError = useMemo(() => {
    if (name.trim() === "") {
      return "Give the environment a name — it is what the list and every log line calls it.";
    }
    if (name.trim().length > 64) {
      return "A name is at most 64 characters.";
    }
    return null;
  }, [name]);

  const hostError = useMemo(() => {
    const trimmed = host.trim();
    if (trimmed === "") {
      return null;
    }
    if (!/^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$/i.test(trimmed)) {
      return "A host is a name, not a URL: staging.example.com, with no scheme and no path.";
    }
    return null;
  }, [host]);

  // Validation counts areas that actually **copy**, not boxes that are ticked. Un-ticking the
  // working areas while leaving the three shared ones ticked used to satisfy this check and then
  // produce an environment with nothing in it — the exact "looks cloned and is empty" state the
  // `require_areas` guard in the crate exists to prevent, reachable through the browser because
  // the panel's rule was a different rule.
  const copyingAreas = useMemo(
    () => areas.filter((area) => area.copies).map((area) => area.name),
    [areas],
  );
  const areaError =
    selected.filter((name) => copyingAreas.includes(name)).length === 0
      ? "Choose at least one thing to copy."
      : null;

  const submit = async () => {
    setBusy(true);
    setError(null);
    try {
      const environment = await createEnvironment({
        name: name.trim(),
        key: key.trim() === "" ? undefined : key.trim(),
        staging_host: host.trim() === "" ? undefined : host.trim(),
        areas: selected,
        exclude_archived: excludeArchived,
      });
      onCreated(environment);
    } catch (cause) {
      // A `400` from here names the field it is about (`key`, `staging_host`, `areas`), and the
      // screen that shows it is the step the operator is standing on — so the message lands
      // where it can be acted on instead of in a corner.
      setError(cause instanceof ApiError ? cause.message : "The environment was not created.");
    } finally {
      setBusy(false);
    }
  };

  const canAdvance = (() => {
    if (step === 0) {
      return nameError === null;
    }
    if (step === 1) {
      return hostError === null;
    }
    if (step === 2) {
      return areaError === null;
    }
    return true;
  })();

  const toggleArea = (value: string) => {
    setSelected((current) =>
      current.includes(value)
        ? current.filter((name_) => name_ !== value)
        : [...current, value],
    );
  };

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-ink/40 p-4"
      onClick={onClose}
      data-env-wizard-overlay
    >
      <div
        ref={dialogRef}
        role="dialog"
        aria-modal="true"
        aria-label="New staging environment"
        data-env-wizard
        onClick={(event) => event.stopPropagation()}
        className="flex max-h-[90vh] w-full max-w-xl flex-col gap-4 overflow-y-auto rounded-xl border border-line bg-panel p-5 shadow-lg"
      >
        <header className="flex items-start justify-between gap-3">
          <div className="flex flex-col gap-1">
            <h2 className="text-[14px] font-semibold text-ink">New staging environment</h2>
            <p className="text-[12px] text-muted">
              {`A copy of ${sourceKey || "production"} you can break safely.`}
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close the wizard"
            data-env-wizard-close
            className="rounded-lg border border-line bg-surface p-1.5 transition hover:bg-canvas"
          >
            <X className="size-3.5" aria-hidden />
          </button>
        </header>

        <ol className="flex flex-wrap items-center gap-2 text-[11.5px]" data-env-wizard-steps>
          {STEPS.map((label, index) => (
            <li key={label} className="flex items-center gap-2">
              <span
                data-env-wizard-step={index}
                data-env-wizard-step-state={
                  index === step ? "current" : index < step ? "done" : "todo"
                }
                className={
                  index === step
                    ? "font-medium text-ink"
                    : index < step
                      ? "text-positive"
                      : "text-muted"
                }
              >
                {index < step ? <Check className="inline size-3" aria-hidden /> : null}
                {label}
              </span>
              {index < STEPS.length - 1 ? <span className="text-muted">→</span> : null}
            </li>
          ))}
        </ol>

        {step === 0 ? (
          <div className="flex flex-col gap-3">
            <label className="flex flex-col gap-1 text-[12.5px]" htmlFor="env-name">
              <span className="font-medium">Name</span>
              <input
                ref={nameRef}
                id="env-name"
                value={name}
                onChange={(event) => setName(event.target.value)}
                data-env-name
                aria-invalid={nameError !== null && name.trim() !== ""}
                className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px]"
                placeholder="Staging"
              />
              {nameError && name.trim() !== "" ? (
                <span className="text-[11.5px] text-accent-strong">{nameError}</span>
              ) : null}
            </label>
            <label className="flex flex-col gap-1 text-[12.5px]" htmlFor="env-key">
              <span className="font-medium">Key (optional)</span>
              <input
                id="env-key"
                value={key}
                onChange={(event) => setKey(event.target.value)}
                data-env-key
                className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px]"
                placeholder="Derived from the name"
              />
              <span className="text-[11.5px] text-muted">
                Lowercase letters, digits and dashes. Left empty, it is derived from the name.
              </span>
            </label>
          </div>
        ) : null}

        {step === 1 ? (
          <div className="flex flex-col gap-3">
            <label className="flex flex-col gap-1 text-[12.5px]" htmlFor="env-host">
              <span className="font-medium">Staging host (optional)</span>
              <input
                id="env-host"
                value={host}
                onChange={(event) => setHost(event.target.value)}
                data-env-host
                aria-invalid={hostError !== null}
                className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px]"
                placeholder="staging.example.com"
              />
              {hostError ? (
                <span className="text-[11.5px] text-accent-strong">{hostError}</span>
              ) : (
                <span className="text-[11.5px] text-muted">
                  Where this environment is served. Leaving it empty is fine while you are only
                  using it for previews.
                </span>
              )}
            </label>
          </div>
        ) : null}

        {step === 2 ? (
          <div className="flex flex-col gap-2">
            <p className="text-[12px] text-muted">What should the first clone carry?</p>
            <ul className="flex flex-col gap-1.5">
              {areas.map((area) => (
                <li key={area.name}>
                  <label
                    className={`flex items-start gap-2.5 rounded-lg border bg-surface px-3 py-2 ${
                      area.copies ? "border-line" : "border-line/60"
                    }`}
                    data-env-area-option={area.name}
                    data-env-area-copies={area.copies}
                  >
                    <input
                      type="checkbox"
                      checked={selected.includes(area.name)}
                      onChange={() => toggleArea(area.name)}
                      data-env-area={area.name}
                      className="mt-0.5"
                    />
                    <span className="flex flex-col">
                      <span
                        className={`text-[12.5px] font-medium ${area.copies ? "" : "text-muted"}`}
                      >
                        {area.label}
                      </span>
                      <span className="text-[11.5px] text-muted">
                        {area.copies ? area.weight : area.note}
                      </span>
                    </span>
                  </label>
                </li>
              ))}
            </ul>
            {/* The boundary, stated once for all three, instead of only in three places a
                reader has to notice. A note under every checkbox would be noise; a note under
                exactly the three that do nothing is the difference between "0 rows because the
                site is empty" and "0 rows because this row is shared with production". */}
            {areas.some((area) => !area.copies) ? (
              <p className="text-[11.5px] text-muted" data-env-area-note>
                Staging shares its navigation, settings and theme with production. Those three
                are listed so you can see what is not being copied, not so you can tick it — a
                staging environment and production always show the same ones.
              </p>
            ) : null}
            <label className="mt-1 flex items-center gap-2 text-[12.5px]">
              <input
                type="checkbox"
                checked={excludeArchived}
                onChange={(event) => setExcludeArchived(event.target.checked)}
                data-env-exclude-archived
              />
              Leave archived pages behind
            </label>
            {areaError ? (
              <span className="text-[11.5px] text-accent-strong">{areaError}</span>
            ) : null}
          </div>
        ) : null}

        {step === 3 ? (
          <div className="flex flex-col gap-2 text-[12.5px]">
            <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1.5">
              <dt className="text-muted">Name</dt>
              <dd className="font-medium">{name.trim()}</dd>
              <dt className="text-muted">Key</dt>
              <dd className="font-mono">{key.trim() === "" ? "derived from the name" : key.trim()}</dd>
              <dt className="text-muted">Host</dt>
              <dd className="font-mono">{host.trim() === "" ? "none" : host.trim()}</dd>
              <dt className="text-muted">From</dt>
              <dd className="font-mono">{sourceKey || "production"}</dd>
              <dt className="text-muted">Content</dt>
              {/* Only the areas that will actually be copied. Listing the shared ones here
                  would repeat the promise the step-2 note withdrew, on the last screen the
                  operator reads before pressing the button. */}
              <dd>
                {areas
                  .filter((area) => area.copies && selected.includes(area.name))
                  .map((area) => area.label)
                  .join(", ") || "nothing"}
              </dd>
            </dl>
            <p className="text-[11.5px] text-muted" data-env-wizard-shared>
              Navigation, settings and theme stay shared with production and are not copied.
            </p>
            <p className="text-[11.5px] text-muted">
              The copy runs in the background. The environment appears on the list immediately, in
              the `cloning` state, and the row shows its progress until it finishes.
            </p>
          </div>
        ) : null}

        {error ? (
          <p role="alert" data-env-wizard-error className="text-[12px] text-accent-strong">
            {error}
          </p>
        ) : null}

        <footer className="flex items-center justify-between gap-2">
          <button
            type="button"
            onClick={() => (step === 0 ? onClose() : setStep((current) => current - 1))}
            data-env-wizard-back
            className="rounded-lg border border-line bg-surface px-3 py-2 text-[12.5px] transition hover:bg-canvas"
          >
            {step === 0 ? "Cancel" : "Back"}
          </button>
          {step < STEPS.length - 1 ? (
            <button
              type="button"
              onClick={() => setStep((current) => current + 1)}
              disabled={!canAdvance}
              data-env-wizard-next
              className="rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
            >
              Next
            </button>
          ) : (
            <button
              type="button"
              onClick={() => void submit()}
              disabled={busy}
              data-env-wizard-submit
              // The generic walkthrough pass clicks every button on the page, and a wizard is the
              // one control that is unsafe to let it drive: the pass fills the name and key with
              // sample values and submits, which creates a real staging environment whose clone
              // then runs, and the screen's own depth pass — the one that knows the key it wants
              // and has to assert the copy afterwards — finds its environment already created with
              // the wrong key and reports "the wizard submitted but no environment with key
              // qa-staging-… exists". The guard is the harness's own mechanism for exactly this
              // ("this control belongs to the screen's own pass"), and the button that creates a
              // tenant-scoped record is the clearest case of it in the panel.
              data-qa-guard="environments-depth"
              className="rounded-lg bg-accent px-3 py-2 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
            >
              {busy ? "Creating…" : "Create and clone"}
            </button>
          )}
        </footer>
      </div>
    </div>
  );
}

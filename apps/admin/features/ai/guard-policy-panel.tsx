"use client";

/**
 * `/ai/guard` — the data guard's policy panel (docs/requests/REQ-105, slice 1).
 *
 * The panel answers one question: **what happens to the text a model is about to see.** Every
 * decision on it is a default, and a default that is wrong is invisible — nothing errors, the
 * model simply receives an e-mail address. Four rules shape the screen because of that:
 *
 * 1. **The default is edited in place and saved explicitly, never on blur.** A per-row `PUT`
 *    that fires when a select loses focus means a mis-click is durable and silent. One Save, a
 *    dirty state, and the button naming what it will do is the difference between a control and
 *    a trapdoor.
 *
 * 2. **The rule set and the policy are two screens, and this one says so.** A label default that
 *    no enabled rule reports under protects nothing, and an operator reading "phone_number: mask"
 *    reasonably concludes phones are masked. The label row links to the rules table filtered to
 *    that label, so the two facts are one click apart instead of two tabs.
 *
 * 3. **"All permissive" is a banner, not a subtle tint.** Every label at `allow` is a valid
 *    starting point — the seeded state — so the screen must not treat it as an error. It is
 *    stated as a fact with the sentence that makes it actionable ("nothing is inspected") rather
 *    than as a red state the operator is nudged out of.
 *
 * 4. **What a label does NOT catch is on the row.** A control whose only documentation is its
 *    name is a control nobody can trust. `person_name` ships disabled because no name list ships
 *    with it, and a panel that printed only "person name" would be describing a capability that
 *    does not exist.
 *
 * The exemption list lives here too, because an exemption that narrows a label default *is* a
 * policy edit, and splitting it from the policy it modifies is how an operator ends up widening
 * a label without seeing the exemption that used to be doing it.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import Link from "next/link";

import { Plus, ShieldAlert, TriangleAlert, X } from "lucide-react";

import {
  ApiError,
  type GuardExemption,
  type GuardLabel,
  type GuardPolicy,
  createGuardExemption,
  deleteGuardExemption,
  fetchGuardExemptions,
  fetchGuardPolicy,
  saveGuardPolicy,
} from "@/lib/guard-api";
import { formatTimestamp } from "@/lib/format";

/**
 * The four actions, in the order the panel draws them, with the sentence that says what choosing
 * one does. The order is **least to most destructive**, because a select's first option is what
 * a keyboard reaches by pressing Enter without looking, and that should not be `block`.
 */
const ACTIONS = [
  { value: "allow", blurb: "Sent through untouched. Nothing about this label is inspected." },
  { value: "flag", blurb: "Sent through, and the match is recorded for review." },
  { value: "mask", blurb: "Sent through with the matched values replaced by placeholders." },
  { value: "block", blurb: "Refused. The provider is never called." },
] as const;

const MASK_STYLES = [
  { value: "numbered", blurb: "Placeholders count up per turn: [EMAIL_1], [EMAIL_2]." },
  { value: "deterministic", blurb: "The same value gets the same placeholder every time." },
] as const;

/** What the form holds while an exemption is being written. */
type ExemptionDraft = {
  label: string;
  providers: string;
  features: string;
  reason: string;
  expires_at: string;
};

const BLANK_EXEMPTION: ExemptionDraft = {
  label: "",
  providers: "",
  features: "",
  reason: "",
  expires_at: "",
};

/** Splits a comma/space separated scope box into a list. */
function scopeList(value: string): string[] {
  return value
    .split(/[\s,]+/)
    .map((item) => item.trim())
    .filter(Boolean);
}

/** Why an exemption the form typed would be refused, or `null` when it is fine. */
export function checkExemption(draft: ExemptionDraft): string | null {
  if (!draft.label.trim()) return "An exemption has to name the label it narrows.";
  if (!draft.reason.trim()) {
    return "An exemption needs a reason — an unexplained gap in a control is invisible later.";
  }
  if (draft.reason.trim().length > 500) {
    return `The reason is ${draft.reason.trim().length} characters; the limit is 500.`;
  }
  return null;
}

/** The policy panel. */
export function GuardPolicyPanel() {
  const [policy, setPolicy] = useState<GuardPolicy | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  // The draft holds what the operator has chosen, which is NOT what is saved. Keeping them in
  // one state object is the bug this screen exists to avoid: a select that writes straight into
  // the saved map makes "unsaved" unrepresentable.
  const [draft, setDraft] = useState<{
    label_defaults: Record<string, string>;
    mask_style: string;
    allow_user_override: boolean;
  } | null>(null);
  const [saving, setSaving] = useState(false);
  const [saved, setSaved] = useState<string | null>(null);

  const [exemptions, setExemptions] = useState<GuardExemption[]>([]);
  /**
   * Whether the list actually arrived.
   *
   * Tracked apart from `exemptions.length` on purpose: a failed list call and a tenant with no
   * exemptions both leave the array empty, and only one of them is an empty state worth
   * printing. Without this flag the screen says "No exemptions" about a list it never read.
   */
  const [exemptionsLoaded, setExemptionsLoaded] = useState(false);
  const [exemptionOpen, setExemptionOpen] = useState(false);
  const [exemptionDraft, setExemptionDraft] = useState<ExemptionDraft>(BLANK_EXEMPTION);
  const [exemptionError, setExemptionError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [confirming, setConfirming] = useState<GuardExemption | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const next = await fetchGuardPolicy();
      setPolicy(next);
      setDraft({
        label_defaults: { ...next.label_defaults },
        mask_style: next.mask_style,
        allow_user_override: next.allow_user_override,
      });
      // The exemption list loads alongside the policy rather than on a second click: a screen
      // that shows a live count of something it cannot list makes the operator go looking for the
      // list, and the honest answer is "it is here".
      try {
        const list = await fetchGuardExemptions();
        setExemptions(list.rows);
        setExemptionsLoaded(true);
      } catch {
        // The policy is still fully usable without the list; the section says so if it is empty.
      }
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  /** Dirty is computed against the loaded policy, never tracked with a flag. */
  const dirty = useMemo(() => {
    if (!policy || !draft) return false;
    const sameDefaults = policy.labels.every(
      (label) => (draft.label_defaults[label.key] ?? "allow") === policy.label_defaults[label.key],
    );
    return (
      !sameDefaults ||
      draft.mask_style !== policy.mask_style ||
      draft.allow_user_override !== policy.allow_user_override
    );
  }, [policy, draft]);

  const save = useCallback(async () => {
    if (!draft) return;
    setSaving(true);
    setError(null);
    setSaved(null);
    try {
      const next = await saveGuardPolicy({
        // The whole map, not a diff: the server treats the field as replacement, so sending only
        // the rows that changed would silently drop the operator's other choices.
        label_defaults: draft.label_defaults,
        mask_style: draft.mask_style,
        allow_user_override: draft.allow_user_override,
      });
      setPolicy(next);
      setDraft({
        label_defaults: { ...next.label_defaults },
        mask_style: next.mask_style,
        allow_user_override: next.allow_user_override,
      });
      setSaved("Policy saved.");
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setSaving(false);
    }
  }, [draft]);

  /**
   * Reload the exemption list.
   *
   * Previously this refreshed only the policy's `active_exemptions` COUNT, which meant the list
   * below it was never populated — an exemption created on this screen appeared to do nothing at
   * all, because the row that proved it existed was missing. A count with no list beside it is
   * the shape of a feature that lies. Both are reloaded together.
   */
  const reloadExemptions = useCallback(async () => {
    try {
      const list = await fetchGuardExemptions();
      setExemptions(list.rows);
      setExemptionsLoaded(true);
      const next = await fetchGuardPolicy();
      setPolicy((current) =>
        current ? { ...current, active_exemptions: next.active_exemptions } : current,
      );
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    }
  }, []);

  const submitExemption = useCallback(async () => {
    const problem = checkExemption(exemptionDraft);
    setExemptionError(problem);
    if (problem) return;
    setBusy(true);
    try {
      await createGuardExemption({
        label: exemptionDraft.label.trim(),
        providers: scopeList(exemptionDraft.providers),
        features: scopeList(exemptionDraft.features),
        reason: exemptionDraft.reason.trim(),
        expires_at: exemptionDraft.expires_at ? new Date(exemptionDraft.expires_at).toISOString() : undefined,
      });
      setExemptionDraft(BLANK_EXEMPTION);
      setExemptionOpen(false);
      setExemptionError(null);
      await reloadExemptions();
    } catch (cause) {
      setExemptionError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  }, [exemptionDraft, reloadExemptions]);

  const removeExemption = useCallback(async () => {
    if (!confirming) return;
    setBusy(true);
    try {
      await deleteGuardExemption(confirming.id);
      setConfirming(null);
      await reloadExemptions();
    } catch (cause) {
      setError(cause instanceof ApiError ? cause.message : String(cause));
    } finally {
      setBusy(false);
    }
  }, [confirming, reloadExemptions]);

  if (loading) {
    return (
      <p className="px-1 py-10 text-center text-[13px] text-muted">
        Loading the guard policy…
      </p>
    );
  }

  if (error && !policy) {
    return (
      <div className="rounded-lg border border-danger/40 bg-danger/5 p-5" role="alert">
        <p className="text-[13.5px] font-medium text-danger">The guard policy could not be loaded.</p>
        <p className="mt-1 text-[12.5px] text-muted">{error}</p>
        <button
          type="button"
          onClick={() => void load()}
          className="mt-3 rounded-md border border-line px-3 py-1.5 text-[12.5px] hover:bg-muted/40"
        >
          Try again
        </button>
      </div>
    );
  }

  if (!policy || !draft) return null;

  const totals = policy.totals ?? {};

  return (
    <div className="flex flex-col gap-6" data-guard-policy>
      {policy.all_permissive ? (
        <div
          role="status"
          className="flex items-start gap-3 rounded-lg border border-warning/40 bg-warning/5 p-4"
        >
          <ShieldAlert aria-hidden className="mt-0.5 size-4 shrink-0 text-warning" />
          <div>
            <p className="text-[13.5px] font-medium">Every label sits at “allow”.</p>
            <p className="mt-1 text-[12.5px] text-muted">
              Nothing is being inspected before a model sees it: no value is flagged, nothing is
              masked, nothing is refused. This is the state a new tenant starts in, and it is a
              legitimate one — but it is not a guard until at least one label is set to flag,
              mask or block.
            </p>
          </div>
        </div>
      ) : null}

      {error ? (
        <p role="alert" className="rounded-md bg-danger/10 px-3 py-2 text-[12.5px] text-danger">
          {error}
        </p>
      ) : null}

      {/* The window the numbers cover, stated once, so no stat card has to carry its own suffix. */}
      <p className="text-[12.5px] text-muted">
        Counts cover the last {policy.window_days} days. Rules that fire decide what happens; a
        label with no enabled rule behind it changes nothing.
      </p>

      <section aria-label="Window totals" className="grid grid-cols-2 gap-3 sm:grid-cols-4">
        <Stat label={`Matches · ${policy.window_days}d`} value={policy.matches} />
        <Stat label="Flagged" value={totals.flagged ?? 0} />
        <Stat label="Masked" value={totals.masked ?? 0} />
        <Stat label="Blocked" value={totals.blocked ?? 0} />
      </section>

      <section
        aria-label="Label defaults"
        className="rounded-lg border border-line"
      >
        <header className="flex flex-wrap items-center justify-between gap-3 border-b border-line px-4 py-3">
          <div>
            <h2 className="text-[14px] font-medium">Default action per label</h2>
            <p className="text-[12px] text-muted">
              {policy.enabled_rules} of {policy.rule_budget} rules enabled.{" "}
              <Link href="/ai/guard/rules" className="underline underline-offset-2 hover:text-ink">
                Open the rules table
              </Link>
              .
            </p>
          </div>
          <div className="flex items-center gap-2">
            {saved ? (
              <span role="status" className="text-[12.5px] text-muted">
                {saved}
              </span>
            ) : null}
            {dirty ? (
              <button
                type="button"
                onClick={() => {
                  setDraft({
                    label_defaults: { ...policy.label_defaults },
                    mask_style: policy.mask_style,
                    allow_user_override: policy.allow_user_override,
                  });
                  setSaved(null);
                }}
                className="rounded-md border border-line px-3 py-1.5 text-[12.5px] hover:bg-muted/40"
              >
                Discard changes
              </button>
            ) : null}
            <button
              type="button"
              onClick={() => void save()}
              data-guard-policy-save
              disabled={!dirty || saving}
              className="rounded-md bg-ink px-3 py-1.5 text-[12.5px] text-bg disabled:opacity-40"
            >
              {saving ? "Saving…" : "Save policy"}
            </button>
          </div>
        </header>

        <ul className="divide-y divide-line">
          {policy.labels.map((label: GuardLabel) => (
            <li
              key={label.key}
              className="flex flex-col gap-3 px-4 py-4 lg:flex-row lg:items-start lg:justify-between"
            >
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2">
                  <h3 className="font-mono text-[13px] font-medium">{label.key}</h3>
                  <span className="rounded bg-muted/60 px-1.5 py-0.5 text-[11px] text-muted">
                    {policy.matches_by_label[label.key] ?? 0} matches
                  </span>
                  {label.validator ? (
                    <span className="rounded bg-muted/60 px-1.5 py-0.5 text-[11px] text-muted">
                      {label.validator}
                    </span>
                  ) : null}
                </div>
                <p className="mt-1 text-[12.5px] text-muted">
                  <span className="text-ink">Catches:</span> {label.catches}
                </p>
                <p className="mt-0.5 text-[12.5px] text-muted">
                  <span className="text-ink">Does not catch:</span> {label.misses}
                </p>
                <Link
                  href={`/ai/guard/rules?label=${encodeURIComponent(label.key)}`}
                  data-guard-label-link={label.key}
                  className="mt-1.5 inline-block text-[12px] underline underline-offset-2 hover:text-ink"
                >
                  Rules reporting under this label
                </Link>
              </div>

              <div className="lg:w-72 lg:shrink-0">
                <label
                  htmlFor={`guard-action-${label.key}`}
                  className="block text-[12px] font-medium"
                >
                  Action for {label.key}
                </label>
                <select
                  id={`guard-action-${label.key}`}
                  data-guard-action={label.key}
                  value={draft.label_defaults[label.key] ?? "allow"}
                  onChange={(event) =>
                    setDraft((current) =>
                      current
                        ? {
                            ...current,
                            label_defaults: {
                              ...current.label_defaults,
                              [label.key]: event.target.value,
                            },
                          }
                        : current,
                    )
                  }
                  className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
                >
                  {ACTIONS.map((action) => (
                    <option key={action.value} value={action.value}>
                      {action.value}
                    </option>
                  ))}
                </select>
                <p className="mt-1 text-[11.5px] text-muted">
                  {ACTIONS.find((a) => a.value === (draft.label_defaults[label.key] ?? "allow"))
                    ?.blurb}
                </p>
              </div>
            </li>
          ))}
        </ul>
      </section>

      <section aria-label="Masking and overrides" className="rounded-lg border border-line p-4">
        <h2 className="text-[14px] font-medium">Masking</h2>
        <p className="mt-1 text-[12.5px] text-muted">
          How a masked value is replaced in the text the provider receives.
        </p>
        <div className="mt-3 max-w-sm">
          <label htmlFor="guard-mask-style" className="block text-[12px] font-medium">
            Mask style
          </label>
          <select
            id="guard-mask-style"
            value={draft.mask_style}
            onChange={(event) =>
              setDraft((current) =>
                current ? { ...current, mask_style: event.target.value } : current,
              )
            }
            className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
          >
            {MASK_STYLES.map((style) => (
              <option key={style.value} value={style.value}>
                {style.value}
              </option>
            ))}
          </select>
          <p className="mt-1 text-[11.5px] text-muted">
            {MASK_STYLES.find((s) => s.value === draft.mask_style)?.blurb}
          </p>
        </div>

        <div className="mt-5 border-t border-line pt-4">
          <label className="flex items-start gap-3">
            <input
              type="checkbox"
              checked={draft.allow_user_override}
              onChange={(event) =>
                setDraft((current) =>
                  current
                    ? { ...current, allow_user_override: event.target.checked }
                    : current,
                )
              }
              className="mt-0.5 size-4"
            />
            <span>
              <span className="block text-[12.5px] font-medium">
                Let users weaken a label for their own calls
              </span>
              <span className="mt-0.5 block text-[12px] text-muted">
                With this on, a user can turn a masked label into an allowed one for a call of
                their own. The override is recorded as a separate event, but the values leave the
                platform unmasked.
              </span>
            </span>
          </label>
        </div>
      </section>

      <section aria-label="Exemptions" className="rounded-lg border border-line">
        <header className="flex flex-wrap items-center justify-between gap-3 border-b border-line px-4 py-3">
          <div>
            <h2 className="text-[14px] font-medium">Exemptions</h2>
            <p className="text-[12px] text-muted">
              {policy.active_exemptions} in force. An exemption narrows a label; it can never
              release a block.
            </p>
          </div>
          <button
            type="button"
            onClick={() => setExemptionOpen((current) => !current)}
            data-guard-exemption-new
            aria-expanded={exemptionOpen}
            className="inline-flex items-center gap-1.5 rounded-md border border-line px-3 py-1.5 text-[12.5px] hover:bg-muted/40"
          >
            <Plus aria-hidden className="size-3.5" />
            New exemption
          </button>
        </header>

        {exemptionOpen ? (
          <div className="border-b border-line bg-muted/20 px-4 py-4">
            <div className="grid gap-3 sm:grid-cols-2">
              <Field
                id="exemption-label"
                label="Label"
                value={exemptionDraft.label}
                placeholder="email_address"
                onChange={(value) =>
                  setExemptionDraft((current) => ({ ...current, label: value }))
                }
              />
              <Field
                id="exemption-expires"
                label="Expires (optional)"
                type="datetime-local"
                value={exemptionDraft.expires_at}
                onChange={(value) =>
                  setExemptionDraft((current) => ({ ...current, expires_at: value }))
                }
              />
              <Field
                id="exemption-providers"
                label="Providers (blank = all)"
                value={exemptionDraft.providers}
                placeholder="commandcode, opencode-go"
                onChange={(value) =>
                  setExemptionDraft((current) => ({ ...current, providers: value }))
                }
              />
              <Field
                id="exemption-features"
                label="Features (blank = all)"
                value={exemptionDraft.features}
                placeholder="crm, billing"
                onChange={(value) =>
                  setExemptionDraft((current) => ({ ...current, features: value }))
                }
              />
            </div>
            <div className="mt-3">
              <Field
                id="exemption-reason"
                label="Reason"
                value={exemptionDraft.reason}
                placeholder="The payment provider needs the full PAN to charge it."
                onChange={(value) =>
                  setExemptionDraft((current) => ({ ...current, reason: value }))
                }
              />
            </div>
            {exemptionError ? (
              <p role="alert" className="mt-3 text-[12.5px] text-danger">
                {exemptionError}
              </p>
            ) : null}
            <div className="mt-3 flex gap-2">
              <button
                type="button"
                onClick={() => void submitExemption()}
                data-guard-exemption-save
                disabled={busy}
                className="rounded-md bg-ink px-3 py-1.5 text-[12.5px] text-bg disabled:opacity-40"
              >
                {busy ? "Saving…" : "Add exemption"}
              </button>
              <button
                type="button"
                onClick={() => {
                  setExemptionOpen(false);
                  setExemptionDraft(BLANK_EXEMPTION);
                  setExemptionError(null);
                }}
                className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
              >
                Cancel
              </button>
            </div>
          </div>
        ) : null}

        <ExemptionRows
          exemptions={exemptions}
          loaded={exemptionsLoaded}
          confirming={confirming}
          onConfirm={setConfirming}
          onRemove={removeExemption}
        />
      </section>
    </div>
  );
}

/**
 * The exemption rows.
 *
 * **Three distinct empty states, not one.** "None configured" and "the list could not be loaded"
 * and "they all lapsed" read identically to an operator deciding whether their exemption still
 * applies, and conflating them is how a lapsed control stays believed to be live. `loaded` and
 * `liveCount` are passed in rather than derived inside, so the component never has to guess.
 */
function ExemptionRows({
  exemptions,
  loaded,
  confirming,
  onConfirm,
  onRemove,
}: {
  exemptions: GuardExemption[];
  loaded: boolean;
  confirming: GuardExemption | null;
  onConfirm: (row: GuardExemption | null) => void;
  onRemove: () => void;
}) {
  if (!loaded) {
    return (
      <p className="px-4 py-6 text-center text-[12.5px] text-muted">
        The exemption list could not be loaded. Reload the screen to try again — the label
        defaults above are unaffected.
      </p>
    );
  }

  if (exemptions.length === 0) {
    return (
      <p className="px-4 py-6 text-center text-[12.5px] text-muted">
        No exemptions. Every label default on this panel is in force for every provider and
        feature.
      </p>
    );
  }

  const liveCount = exemptions.filter((row) => row.live).length;
  return (
    <div>
      {liveCount === 0 ? (
        <p
          role="status"
          className="flex items-center gap-2 border-b border-line px-4 py-2 text-[12.5px] text-muted"
        >
          <TriangleAlert aria-hidden className="size-3.5 text-warning" />
          None of these are in force — every one has lapsed.
        </p>
      ) : null}
      <ul className="divide-y divide-line">
        {exemptions.map((row) => (
          <li key={row.id} className="flex flex-wrap items-start justify-between gap-3 px-4 py-3">
            <div className="min-w-0">
              <p className="text-[13px] font-medium">
                {row.label}
                {row.live ? null : (
                  <span className="ml-2 rounded bg-muted/60 px-1.5 py-0.5 text-[11px] text-muted">
                    lapsed
                  </span>
                )}
              </p>
              <p className="text-[12px] text-muted">{row.reason}</p>
              <p className="text-[11.5px] text-muted">
                {formatTimestamp(row.created_at)}
                {row.expires_at ? ` → ${formatTimestamp(row.expires_at)}` : ""}
              </p>
            </div>
            {confirming?.id === row.id ? (
              <div className="flex items-center gap-2">
                <button
                  type="button"
                  onClick={onRemove}
                  className="rounded-md bg-danger px-2.5 py-1 text-[12px] text-white"
                >
                  Confirm
                </button>
                <button
                  type="button"
                  onClick={() => onConfirm(null)}
                  className="rounded-md border border-line px-2.5 py-1 text-[12px]"
                >
                  Cancel
                </button>
              </div>
            ) : (
              <button
                type="button"
                onClick={() => onConfirm(row)}
                aria-label={`Withdraw the ${row.label} exemption`}
                className="rounded-md border border-line p-1.5 hover:bg-muted/40"
              >
                <X aria-hidden className="size-3.5" />
              </button>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}

/** One window total. */
function Stat({ label, value }: { label: string; value: number }) {
  return (
    <div className="rounded-lg border border-line px-4 py-3">
      <p className="text-[12px] text-muted">{label}</p>
      <p className="mt-0.5 text-[20px] font-medium tabular-nums">{value}</p>
    </div>
  );
}

/** A labelled input. Every field on this screen has a visible label — never a placeholder alone. */
function Field({
  id,
  label,
  value,
  onChange,
  placeholder,
  type = "text",
}: {
  id: string;
  label: string;
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  type?: string;
}) {
  return (
    <div>
      <label htmlFor={id} className="block text-[12px] font-medium">
        {label}
      </label>
      <input
        id={id}
        type={type}
        value={value}
        placeholder={placeholder}
        onChange={(event) => onChange(event.target.value)}
        className="mt-1 w-full rounded-md border border-line bg-bg px-2 py-1.5 text-[12.5px]"
      />
    </div>
  );
}
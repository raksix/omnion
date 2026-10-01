"use client";

/**
 * `/ai/settings/airgap` — the switch that makes "never leaves this machine" a checked fact
 * (REQ-106, slice 2).
 *
 * Every control here exists because the naive version of it is actively dangerous:
 *
 * 1. **The confirmation lists what stops, computed server-side.** A sheet reading "are you sure?"
 *    is a rubber stamp. The list comes from `providers_that_would_block` — the same function the
 *    call path asks — so it cannot drift and cannot under-report. Type-to-confirm sits on top: an
 *    operator who never read the list still cannot finish the sentence by muscle memory.
 *
 * 2. **A reason is required on the way ON and ignored on the way OFF.** That asymmetry is the
 *    request's, and it is the whole reason the control is trustworthy: ON writes the audit row an
 *    auditor reads six months later, while OFF is an emergency action that must never be blocked by
 *    a validation rule. So the form *knows which direction it is* and hides the field rather than
 *    showing a box the API would ignore.
 *
 * 3. **The banner has two tones and `failed` outranks `blocked`.** A failed egress verification
 *    means a call escaped the gap. A green banner over that is precisely the false reassurance
 *    this control exists to prevent.
 *
 * 4. **The allow-list editor sits on the screen as the check it changes.** An operator who has to
 *    hunt for a second screen to add an internal host will instead decide the host is "almost
 *    local" — the exact confusion the list is supposed to remove.
 *
 * 5. **The actor and time survive the gap being turned off.** The row is the history of the
 *    switch; nulling it would make "was this ever on, and who did it" unanswerable.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import {
  AlertTriangle,
  CheckCircle2,
  Loader2,
  Lock,
  Plus,
  RefreshCw,
  ShieldCheck,
  Trash2,
  X,
} from "lucide-react";

import { ApiError } from "@/lib/api";
import {
  REASON_MAX,
  REASON_MIN,
  addAirgapHost,
  fetchAirgap,
  removeAirgapHost,
  setAirgap,
  verifyAirgap,
  type AirgapHost,
  type AirgapOverview,
  type EgressOutcome,
} from "@/lib/airgap-api";
import { formatTimestamp } from "@/lib/format";

/**
 * What a stored verification result renders as.
 *
 * `null` is a third thing, not a pass — and so is `undetermined`, which the check writes when the
 * attempt never reached the switch. Both fall to the "never" branch on purpose: the request calls
 * a failed verification "the loudest alert in this request", and an unrun check is at least as
 * worth saying out loud. Rendering either green would be the same lie in softer clothes.
 *
 * The three words come from `EgressOutcome`, and the panel reads them rather than inventing its
 * own: it used to test for `"passed"`, which the checker has never written, so the reassuring
 * tone was unreachable and a breach rendered as "Never verified" — the most dangerous possible
 * direction for that bug to point.
 */
function verifyTone(result: EgressOutcome | null): { label: string; tone: string } {
  switch (result) {
    // A refusal IS the pass. The label says so, because an operator who sees "Blocked" next to a
    // green pill needs to be told that means working, not broken.
    case "blocked":
      return { label: "Verified — a call was refused", tone: "bg-positive-soft text-positive" };
    case "escaped":
      return { label: "A call escaped", tone: "bg-danger-soft text-danger" };
    default:
      return { label: "Never verified", tone: "bg-quiet-soft text-muted" };
  }
}

export function AirgapSettingsView() {
  const [data, setData] = useState<AirgapOverview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Open means "an operator asked to flip the switch and has not committed yet" — kept apart from
  // the form so a cancelled confirmation does not clear a reason they spent thirty seconds on.
  const [confirming, setConfirming] = useState<"on" | "off" | null>(null);
  const [reason, setReason] = useState("");
  const [typed, setTyped] = useState("");
  const [acknowledged, setAcknowledged] = useState(false);
  const [saving, setSaving] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [saved, setSaved] = useState<string | null>(null);

  const [hostInput, setHostInput] = useState("");
  const [hostNote, setHostNote] = useState("");
  const [hostBusy, setHostBusy] = useState(false);
  const [hostError, setHostError] = useState<string | null>(null);
  const [hostMessage, setHostMessage] = useState<string | null>(null);
  /** Per-row feedback, so one removal's message does not land on another row. */
  const [hostRowState, setHostRowState] = useState<Record<string, string>>({});

  const [verifying, setVerifying] = useState(false);
  const [verifyMessage, setVerifyMessage] = useState<string | null>(null);
  /**
   * What the run just now measured, so the sentence can be tinted.
   *
   * Kept separate from `verifyMessage` on purpose: the message is the API's sentence and must be
   * shown verbatim, while the outcome is what decides the colour. Collapsing them into one string
   * would mean choosing a tone by parsing English, which is how a breach ends up rendered calmly.
   */
  const [verifyOutcome, setVerifyOutcome] = useState<EgressOutcome | null>(null);
  const [verifyError, setVerifyError] = useState<string | null>(null);

  const reasonRef = useRef<HTMLTextAreaElement | null>(null);

  const load = useCallback(() => {
    setBusy(true);
    setError(null);
    fetchAirgap()
      .then((answer) => {
        setData(answer);
      })
      .catch((cause: unknown) => {
        setData(null);
        setError(
          cause instanceof ApiError ? cause.message : "The air-gap state could not be loaded.",
        );
      })
      .finally(() => setBusy(false));
  }, []);

  /**
   * Re-read the switch, clearing whatever the last verification said.
   *
   * Separate from `load` for one reason: `runVerify` calls `load` right after recording its own
   * result, and a `load` that cleared the message would wipe the sentence the operator just asked
   * for — the fetch resolves *after* the message is set, so the button would appear to work and
   * print nothing. Clearing is a navigation action, not a consequence of every read.
   */
  const reloadClearingNotice = useCallback(() => {
    setVerifyMessage(null);
    setVerifyOutcome(null);
    setVerifyError(null);
    load();
  }, [load]);

  useEffect(load, [load]);

  const enabled = data?.state.enabled ?? false;
  const blocked = data?.would_block ?? [];
  const hosts = data?.hosts ?? [];

  /**
   * Fixed to this installation's own name for the action rather than a generic "ENABLE", so the
   * control cannot be completed by muscle memory on a screen full of switches.
   */
  const confirmPhrase = "TURN THE AIR GAP ON";

  /** Whether the ON path may be submitted, and why not when it may not. */
  const onBlocked = useMemo(() => {
    const length = reason.trim().length;
    if (length < REASON_MIN) return `A reason of ${REASON_MIN}–${REASON_MAX} characters is required.`;
    if (length > REASON_MAX) return `The reason is ${length} characters; the maximum is ${REASON_MAX}.`;
    if (blocked.length > 0 && !acknowledged) return "Acknowledge the list of what stops working.";
    if (typed.trim().toUpperCase() !== confirmPhrase) return `Type ${confirmPhrase} to confirm.`;
    return null;
  }, [reason, blocked.length, acknowledged, typed, confirmPhrase]);

  const openConfirmation = useCallback((direction: "on" | "off") => {
    setConfirming(direction);
    setFormError(null);
    setSaved(null);
    // The typed phrase always starts empty and a stale acknowledgement is never carried over:
    // either would let a second flip through a control the operator did not re-read.
    setTyped("");
    setAcknowledged(false);
    if (direction === "on") {
      setReason("");
      window.setTimeout(() => reasonRef.current?.focus(), 0);
    }
  }, []);

  const closeConfirmation = useCallback(() => {
    setConfirming(null);
    setFormError(null);
  }, []);

  const submit = useCallback(async () => {
    if (!confirming) return;
    setSaving(true);
    setFormError(null);
    setSaved(null);
    try {
      const answer = await setAirgap({
        enabled: confirming === "on",
        reason: confirming === "on" ? reason.trim() : null,
        acknowledged: confirming === "on" ? acknowledged : false,
      });
      setData(answer);
      setConfirming(null);
      setReason("");
      setTyped("");
      setSaved(
        confirming === "on"
          ? "The air gap is on. Non-local calls are refused before they leave this host."
          : "The air gap is off. Non-local providers answer again.",
      );
    } catch (cause: unknown) {
      // The API's sentence, not a generic one: `airgap_ack_required` names the providers, and the
      // reason rule says why the field exists at all.
      setFormError(cause instanceof ApiError ? cause.message : "The switch could not be changed.");
    } finally {
      setSaving(false);
    }
  }, [confirming, reason, acknowledged]);

  const addHost = useCallback(async () => {
    const host = hostInput.trim();
    if (!host) return;
    setHostBusy(true);
    setHostError(null);
    setHostMessage(null);
    try {
      const answer = await addAirgapHost({ host, note: hostNote.trim() || null });
      setHostInput("");
      setHostNote("");
      setHostMessage(
        answer.unchanged === true
          ? `${host} was already on the internal allow-list.`
          : `${host} now counts as an internal host. Providers on it are not refused.`,
      );
      // The allow-list changed, so the last verification's verdict is about a configuration that
      // no longer exists — it is cleared rather than left sitting above a stale sentence.
      reloadClearingNotice();
    } catch (cause: unknown) {
      setHostError(
        cause instanceof ApiError ? cause.message : "The host could not be added to the allow-list.",
      );
    } finally {
      setHostBusy(false);
    }
  }, [hostInput, hostNote, reloadClearingNotice]);

  const removeHost = useCallback(
    async (host: AirgapHost) => {
      setHostRowState((state) => ({ ...state, [host.id]: "" }));
      try {
        await removeAirgapHost(host.id);
        setHostMessage(
          `${host.host} is off the allow-list again. A provider on it is now refused while the air gap is on.`,
        );
        // Same reason as adding: the verdict belonged to a configuration that just changed.
        reloadClearingNotice();
      } catch (cause: unknown) {
        setHostRowState((state) => ({
          ...state,
          [host.id]: cause instanceof ApiError ? cause.message : "The host could not be removed.",
        }));
      }
    },
    [reloadClearingNotice],
  );

  const runVerify = useCallback(async () => {
    setVerifying(true);
    setVerifyError(null);
    setVerifyMessage(null);
    setVerifyOutcome(null);
    try {
      const answer = await verifyAirgap();
      setVerifyMessage(answer.message);
      // The outcome is kept so the sentence can be tinted by what it says. A breach rendered in
      // the same quiet grey as a pass is the failure mode this whole screen exists to prevent —
      // the API distinguishes them perfectly and the UI was throwing that distinction away.
      setVerifyOutcome(answer.outcome);
      // The check does not change the switch, but it does change what the banner is allowed to
      // claim — so the whole screen is re-read rather than patched locally.
      load();
    } catch (cause: unknown) {
      setVerifyError(
        cause instanceof ApiError ? cause.message : "The egress verification could not be run.",
      );
    } finally {
      setVerifying(false);
    }
  }, [load]);

  // Escape closes the sheet, and it is the only shortcut here: a compliance switch must not be
  // flippable from the keyboard without the operator reading the sheet.
  useEffect(() => {
    if (!confirming) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !saving) closeConfirmation();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [confirming, saving, closeConfirmation]);

  if (error) {
    return (
      <div data-airgap-error className="flex flex-col gap-3">
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

  if (!data) {
    return (
      <div data-airgap-loading className="flex flex-col gap-3">
        <div className="h-24 animate-pulse rounded-xl border border-line bg-surface" />
        <div className="h-40 animate-pulse rounded-xl border border-line bg-surface" />
      </div>
    );
  }

  const verify = verifyTone(data.state.egress_verify_result);
  // `failed` is the only state in which the gap is on AND the banner is a warning. `blocked` is a
  // working gap; rendering it as an alert would teach operators to dismiss the one banner that
  // means something.
  const bannerTone =
    data.banner.tone === "failed"
      ? "border-danger/40 bg-danger-soft text-danger"
      : "border-line bg-surface text-ink";
  const BannerIcon = data.banner.tone === "failed" ? AlertTriangle : Lock;

  return (
    <div data-airgap-settings className="flex flex-col gap-5">
      {/* The banner. Same words on every AI screen, so an operator learns one sentence. */}
      {data.banner.active ? (
        <div
          data-airgap-banner
          data-tone={data.banner.tone}
          role="status"
          className={`flex flex-wrap items-start gap-3 rounded-xl border p-4 ${bannerTone}`}
        >
          <BannerIcon aria-hidden size={16} className="mt-0.5 shrink-0" />
          <div className="min-w-0 flex-1">
            <p className="text-[13px] font-medium">Air gap on</p>
            <p className="mt-1 text-[12.5px]">{data.banner.message}</p>
          </div>
        </div>
      ) : null}

      {saved ? (
        <p
          data-airgap-saved
          role="status"
          className="rounded-lg bg-positive-soft px-3 py-2 text-[12.5px] text-positive"
        >
          {saved}
        </p>
      ) : null}

      {/* ------------------------------------------------------------------ the switch ---- */}
      <section className="rounded-xl border border-line bg-surface p-4">
        <div className="flex flex-wrap items-start justify-between gap-4">
          <div className="min-w-0 flex-1">
            <h2 className="text-[14px] font-medium text-ink">Air-gap mode</h2>
            <p className="mt-1 max-w-2xl text-[12.5px] text-muted">
              When this is on, every call whose resolved provider is not local is refused
              <em> before</em> any network request leaves this host. The refusal names the provider
              and the host, and it is written to the call log as a blocked attempt — refusing is the
              switch working, not an outage.
            </p>
          </div>
          <button
            type="button"
            data-airgap-toggle
            onClick={() => openConfirmation(enabled ? "off" : "on")}
            className={`inline-flex shrink-0 items-center gap-2 rounded-lg px-3 py-2 text-[12.5px] font-medium text-white transition ${
              enabled ? "bg-danger" : "bg-accent hover:bg-accent-strong"
            }`}
          >
            {enabled ? <X aria-hidden size={14} /> : <ShieldCheck aria-hidden size={14} />}
            {enabled ? "Turn the air gap off" : "Turn the air gap on"}
          </button>
        </div>

        <dl className="mt-4 grid gap-3 sm:grid-cols-3">
          <div>
            <dt className="text-[11.5px] text-muted">Current state</dt>
            <dd data-airgap-state className="mt-0.5 text-[13px] text-ink">
              {enabled ? "On" : "Off"}
            </dd>
          </div>
          <div>
            <dt className="text-[11.5px] text-muted">Last turned on</dt>
            {/* Shown even while the gap is off, on purpose — see the module docs. */}
            <dd className="mt-0.5 text-[13px] text-ink">
              {data.state.enabled_at ? formatTimestamp(data.state.enabled_at) : "Never in effect"}
            </dd>
          </div>
          <div>
            <dt className="text-[11.5px] text-muted">Non-local providers refused</dt>
            <dd className="mt-0.5 text-[13px] text-ink">
              {enabled ? blocked.length : "— (the gap is off, nothing is refused)"}
            </dd>
          </div>
        </dl>

        {data.state.reason ? (
          <div className="mt-3">
            <p className="text-[11.5px] text-muted">Recorded reason</p>
            <p data-airgap-reason className="mt-0.5 text-[12.5px] text-ink">
              {data.state.reason}
            </p>
          </div>
        ) : null}

        {enabled && data.still_available.length > 0 ? (
          <p className="mt-3 rounded-lg bg-positive-soft px-3 py-2 text-[12px] text-positive">
            Still answering while the gap is on: {data.still_available.join(", ")}.
          </p>
        ) : null}
        {enabled && data.still_available.length === 0 ? (
          <p
            data-airgap-no-local
            className="mt-3 rounded-lg bg-caution-soft px-3 py-2 text-[12px] text-caution"
          >
            No local endpoint is registered, so nothing can answer while the gap is on. Register one
            on the Local AI screen before you rely on this.
          </p>
        ) : null}
      </section>

      {/* ------------------------------------------------- what stops, and what still runs ---- */}
      <section className="rounded-xl border border-line bg-surface p-4">
        <h2 className="text-[14px] font-medium text-ink">What the gap stops</h2>
        <p className="mt-1 text-[12.5px] text-muted">
          Computed by the same check the call path runs, so this list cannot drift from the
          behaviour and cannot under-report.
        </p>
        {blocked.length === 0 ? (
          <p data-airgap-nothing-blocked className="mt-3 text-[12.5px] text-muted">
            No non-local provider is registered. Turning the gap on changes nothing until one is.
          </p>
        ) : (
          <ul data-airgap-blocked-list className="mt-3 flex flex-col gap-1.5">
            {blocked.map((provider) => (
              <li
                key={provider.base_url}
                data-airgap-blocked-row
                className="flex flex-wrap items-baseline gap-x-2 text-[12.5px]"
              >
                <span className="font-medium text-ink">{provider.name}</span>
                <span className="text-muted">{provider.base_url}</span>
                <span className="text-muted">— refused</span>
              </li>
            ))}
          </ul>
        )}
      </section>

      {/* ------------------------------------------------------ the internal allow-list ---- */}
      <section className="rounded-xl border border-line bg-surface p-4">
        <h2 className="text-[14px] font-medium text-ink">Internal hosts</h2>
        <p className="mt-1 max-w-2xl text-[12.5px] text-muted">
          Hosts this installation may treat as internal. The allow-list <em>widens</em> the locality
          rule — loopback and private ranges always count — it never replaces it. Removing a host
          takes the answer away again, which is why a host added by mistake is a five-second fix
          rather than a rebuild.
        </p>

        <form
          data-airgap-host-form
          onSubmit={(event) => {
            event.preventDefault();
            void addHost();
          }}
          className="mt-3 flex flex-col gap-2 sm:flex-row"
        >
          <label className="flex flex-1 flex-col gap-1 text-[12px] text-muted">
            Host
            <input
              value={hostInput}
              onChange={(event) => setHostInput(event.target.value)}
              placeholder="llm.internal.example"
              aria-label="Internal host"
              className="rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink"
            />
          </label>
          <label className="flex flex-1 flex-col gap-1 text-[12px] text-muted">
            Note <span className="text-muted">(optional)</span>
            <input
              value={hostNote}
              onChange={(event) => setHostNote(event.target.value)}
              placeholder="Inference box in the server room"
              aria-label="Host note"
              className="rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink"
            />
          </label>
          <button
            type="submit"
            disabled={hostBusy || !hostInput.trim()}
            className="mt-auto inline-flex h-[38px] items-center justify-center gap-1.5 rounded-lg border border-line px-3 text-[12px] transition hover:bg-canvas disabled:opacity-50"
          >
            {hostBusy ? (
              <Loader2 aria-hidden size={14} className="animate-spin" />
            ) : (
              <Plus aria-hidden size={14} />
            )}
            Add host
          </button>
        </form>
        <p className="mt-1.5 text-[11.5px] text-muted">
          A bare host or address only — no scheme, no port, no path. A pasted URL widens nothing
          while appearing to succeed.
        </p>

        {hostError ? (
          <p
            data-airgap-host-error
            className="mt-3 rounded-lg bg-danger-soft px-3 py-2 text-[12px] text-danger"
          >
            {hostError}
          </p>
        ) : null}
        {hostMessage ? (
          <p
            data-airgap-host-message
            role="status"
            className="mt-3 rounded-lg bg-quiet-soft px-3 py-2 text-[12px] text-ink"
          >
            {hostMessage}
          </p>
        ) : null}

        {hosts.length === 0 ? (
          <p data-airgap-hosts-empty className="mt-3 text-[12.5px] text-muted">
            No internal hosts are allow-listed. Loopback and private-range addresses still count.
          </p>
        ) : (
          <ul data-airgap-hosts className="mt-3 flex flex-col gap-1.5">
            {hosts.map((host) => (
              <li
                key={host.id}
                data-airgap-host-row
                className="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-line px-3 py-2"
              >
                <div className="min-w-0">
                  <p className="truncate text-[13px] text-ink">{host.host}</p>
                  <p className="text-[11.5px] text-muted">
                    {host.note ? host.note : "No note"} · added {formatTimestamp(host.created_at)}
                  </p>
                  {hostRowState[host.id] ? (
                    <p className="mt-1 text-[11.5px] text-danger">{hostRowState[host.id]}</p>
                  ) : null}
                </div>
                <button
                  type="button"
                  onClick={() => void removeHost(host)}
                  aria-label={`Remove ${host.host} from the allow-list`}
                  className="inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] transition hover:bg-canvas"
                >
                  <Trash2 aria-hidden size={13} />
                  Remove
                </button>
              </li>
            ))}
          </ul>
        )}
      </section>

      {/* ------------------------------------------------------ the egress verification ---- */}
      <section className="rounded-xl border border-line bg-surface p-4">
        <h2 className="text-[14px] font-medium text-ink">Egress verification</h2>
        <p className="mt-1 max-w-2xl text-[12.5px] text-muted">
          The check the request asks for: with the gap on, attempt a documented non-local call and
          expect the refusal. A refusal is a <strong>pass</strong>; a success is a loud failure and
          turns the banner red.
        </p>
        <div className="mt-3 flex flex-wrap items-center gap-3">
          <span
            data-airgap-verify-result
            data-result={data.state.egress_verify_result ?? "never"}
            className={`inline-flex items-center gap-1.5 rounded-full px-2.5 py-1 text-[11.5px] font-medium ${verify.tone}`}
          >
            {data.state.egress_verify_result === "blocked" ? (
              <CheckCircle2 aria-hidden size={12} />
            ) : (
              <AlertTriangle aria-hidden size={12} />
            )}
            {verify.label}
          </span>
          <span className="text-[12px] text-muted">
            {data.state.egress_verified_at
              ? `Last run ${formatTimestamp(data.state.egress_verified_at)}${
                  data.state.egress_verify_target
                    ? ` against ${data.state.egress_verify_target}`
                    : ""
                }`
              : "It has never been run, so the gap has never been proven from the outside."}
          </span>
          <button
            type="button"
            data-airgap-verify
            onClick={() => void runVerify()}
            disabled={verifying || !enabled}
            className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12px] transition hover:bg-canvas disabled:opacity-50"
          >
            {verifying ? (
              <Loader2 aria-hidden size={14} className="animate-spin" />
            ) : (
              <RefreshCw aria-hidden size={14} />
            )}
            Run the check
          </button>
        </div>
        {!enabled ? (
          <p className="mt-2 text-[11.5px] text-muted">
            There is nothing to verify while every non-local call is permitted.
          </p>
        ) : null}
        {verifyMessage ? (
          <p
            data-airgap-verify-message
            data-outcome={verifyOutcome ?? "none"}
            role="status"
            className={`mt-3 rounded-lg px-3 py-2 text-[12px] ${
              // The colour follows the OUTCOME, never the shape of the sentence. An escaped call
              // is the loudest alert this request has, so it cannot arrive in the same neutral
              // grey as a refusal — which would make the screen's colour contradict its badge
              // directly above it.
              verifyOutcome === "escaped"
                ? "bg-danger-soft text-danger"
                : verifyOutcome === "blocked"
                  ? "bg-positive-soft text-positive"
                  : "bg-quiet-soft text-muted"
            }`}
          >
            {verifyMessage}
          </p>
        ) : null}
        {verifyError ? (
          <p
            data-airgap-verify-error
            className="mt-3 rounded-lg bg-danger-soft px-3 py-2 text-[12px] text-danger"
          >
            {verifyError}
          </p>
        ) : null}
      </section>

      {/* --------------------------------------------------------- the confirmation sheet ---- */}
      {confirming ? (
        <div
          data-airgap-confirm-overlay
          className="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-ink/40 p-4 sm:items-center"
        >
          <div
            role="dialog"
            aria-modal="true"
            aria-label={confirming === "on" ? "Turn the air gap on" : "Turn the air gap off"}
            data-airgap-confirm
            className="my-auto flex w-full max-w-xl flex-col gap-4 rounded-xl border border-line bg-surface p-5"
          >
            <div className="flex items-start justify-between gap-3">
              <div>
                <h2 className="text-[15px] font-semibold text-ink">
                  {confirming === "on" ? "Turn the air gap on?" : "Turn the air gap off?"}
                </h2>
                <p className="mt-1 text-[12.5px] text-muted">
                  {confirming === "on"
                    ? "Every call whose provider is not local will be refused before it leaves this host."
                    : "Non-local providers answer again, and this installation can reach the internet."}
                </p>
              </div>
              <button
                type="button"
                onClick={closeConfirmation}
                disabled={saving}
                aria-label="Close"
                className="rounded-lg border border-line p-1.5 transition hover:bg-canvas disabled:opacity-50"
              >
                <X aria-hidden size={14} />
              </button>
            </div>

            {confirming === "on" ? (
              <>
                <div>
                  <p className="text-[12px] font-medium text-ink">
                    {blocked.length === 0
                      ? "No provider will be refused"
                      : `${blocked.length} provider${
                          blocked.length === 1 ? "" : "s"
                        } will stop answering`}
                  </p>
                  {blocked.length === 0 ? (
                    <p className="mt-1 text-[12px] text-muted">
                      No non-local provider is registered, so nothing changes today. A provider added
                      later will be refused the moment it is used.
                    </p>
                  ) : (
                    <ul className="mt-2 flex flex-col gap-1">
                      {blocked.map((provider) => (
                        <li key={provider.base_url} className="text-[12px] text-muted">
                          <span className="text-ink">{provider.name}</span> — {provider.base_url}
                        </li>
                      ))}
                    </ul>
                  )}
                </div>

                <label className="flex flex-col gap-1 text-[12px] text-muted">
                  Reason{" "}
                  <span className="text-muted">
                    (required, {REASON_MIN}–{REASON_MAX} characters)
                  </span>
                  <textarea
                    ref={reasonRef}
                    value={reason}
                    onChange={(event) => setReason(event.target.value)}
                    rows={3}
                    aria-label="Reason for turning the air gap on"
                    placeholder="PCI-DSS scope: customer records must not leave this host"
                    className="rounded-lg border border-line bg-canvas px-3 py-2 text-[13px] text-ink"
                  />
                  <span className="text-[11.5px]">
                    {reason.trim().length < REASON_MIN
                      ? `${
                          REASON_MIN - reason.trim().length
                        } more characters. This row is what an auditor reads later.`
                      : `${reason.trim().length} / ${REASON_MAX}`}
                  </span>
                </label>

                {blocked.length > 0 ? (
                  <label className="flex items-start gap-2 text-[12.5px] text-ink">
                    <input
                      type="checkbox"
                      data-airgap-ack
                      checked={acknowledged}
                      onChange={(event) => setAcknowledged(event.target.checked)}
                      className="mt-0.5 size-4 rounded border-line"
                    />
                    <span>
                      I understand that the {blocked.length} provider
                      {blocked.length === 1 ? "" : "s"} listed above will stop answering, and that
                      features depending on {blocked.length === 1 ? "it" : "them"} will fail with a
                      clear refusal.
                    </span>
                  </label>
                ) : null}

                <label className="flex flex-col gap-1 text-[12px] text-muted">
                  Type <strong className="text-ink">{confirmPhrase}</strong> to confirm
                  <input
                    value={typed}
                    onChange={(event) => setTyped(event.target.value)}
                    aria-label="Type to confirm"
                    autoComplete="off"
                    spellCheck={false}
                    className="rounded-lg border border-line bg-canvas px-3 py-2 font-mono text-[13px] text-ink"
                  />
                </label>
              </>
            ) : (
              <div className="rounded-lg bg-caution-soft px-3 py-2 text-[12px] text-caution">
                No reason is required to turn the gap off, on purpose: this is the emergency action,
                and a control that can fail closed at the worst moment is not a control. The reason
                the gap was on stays on the record.
              </div>
            )}

            {formError ? (
              <p
                data-airgap-form-error
                className="rounded-lg bg-danger-soft px-3 py-2 text-[12px] text-danger"
              >
                {formError}
              </p>
            ) : null}

            <div className="flex flex-wrap items-center gap-2">
              <button
                type="button"
                data-airgap-confirm-submit
                onClick={() => void submit()}
                disabled={saving || (confirming === "on" && onBlocked !== null)}
                className={`inline-flex items-center gap-1.5 rounded-lg px-3 py-2 text-[12.5px] font-medium text-white disabled:opacity-50 ${
                  confirming === "on" ? "bg-accent hover:bg-accent-strong" : "bg-danger"
                }`}
              >
                {saving ? <Loader2 aria-hidden size={14} className="animate-spin" /> : null}
                {confirming === "on" ? "Turn it on" : "Turn it off"}
              </button>
              <button
                type="button"
                onClick={closeConfirmation}
                disabled={saving}
                className="rounded-lg border border-line px-3 py-2 text-[12.5px] transition hover:bg-canvas disabled:opacity-50"
              >
                Cancel
              </button>
              {confirming === "on" && onBlocked ? (
                <span data-airgap-confirm-blocked className="text-[11.5px] text-muted">
                  {onBlocked}
                </span>
              ) : null}
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}
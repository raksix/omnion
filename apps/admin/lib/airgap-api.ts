/**
 * The air-gap switch client (docs/requests/REQ-106, slice 2).
 *
 * A module of its own for one reason: **`/ai/settings/airgap` is the screen an operator opens
 * during an incident**, and it must not depend on a module whose other job is loading model lists.
 * A screen that is unavailable because an unrelated import changed is a screen that is unavailable
 * exactly when it is needed.
 *
 * **The confirmation list is never built here.** `would_block` arrives computed, from the same
 * function the call path asks. A client-side version would own a second copy of the locality rule
 * and could disagree with the check — under-reporting the providers that will stop, which is the
 * direction that actually hurts.
 */
import { request } from "./api";

/**
 * What one egress-verification attempt measured.
 *
 * The names describe the ATTEMPT, not the verdict, because a refusal is the pass:
 *
 * - `blocked` — the call was refused. **This is the pass.**
 * - `escaped` — the call was permitted and left. A breach, and the loudest alert in this request.
 * - `undetermined` — the attempt never reached the check, so it proved nothing. NOT a pass.
 *
 * A union rather than `string` on purpose: the screen's badge and the checker's stored word must
 * not drift, and a widened type is what let them drift in the first place — the panel used to
 * test for `"passed"`, a word the checker has never written, so the green tone was unreachable
 * and a breach rendered as "Never verified".
 */
export type EgressOutcome = "blocked" | "escaped" | "undetermined";

/** The switch, as the panel reads it. */
export type AirgapState = {
  /** Whether non-local calls are refused right now. */
  enabled: boolean;
  /** Why it was on. Kept after it is turned off — the row is the history of the switch. */
  reason: string | null;
  /** ISO-8601 when it was turned on, or `null` if it never has been. */
  enabled_at: string | null;
  /** Whether the operator acknowledged the list of providers that will stop working. */
  low_confidence_ack: boolean;
  /** ISO-8601 of the last egress verification, or `null` when it has never run. */
  egress_verified_at: string | null;
  /** The host the last verification aimed at. */
  egress_verify_target: string | null;
  /**
   * The last attempt's outcome, or `null` when no verification has run.
   *
   * Same three words as {@link EgressOutcome}, stored rather than re-derived: the banner reads
   * this column, so it must mean exactly what the checker wrote.
   */
  egress_verify_result: EgressOutcome | null;
  /** ISO-8601 of the last write to this row, for any reason. */
  updated_at: string;
};

/**
 * One provider the gap would refuse.
 *
 * `base_url` rather than the bare host, deliberately: a confirmation row links to the provider it
 * will stop, and only the *refusal* carries a bare host — because only the refusal is read by
 * someone who is not already on the providers screen.
 */
export type BlockedProvider = {
  name: string;
  base_url: string;
};

/** One allow-list row. */
export type AirgapHost = {
  /** Row id, for the delete call. */
  id: string;
  /** The host as stored, lowercased. */
  host: string;
  /** Why the operator added it. */
  note: string | null;
  /** ISO-8601. */
  created_at: string;
};

/**
 * The banner every AI screen shows while the gap is on.
 *
 * `tone` is the branch the UI must respect: `blocked` is a working gap, `failed` means the last
 * egress verification let a call escape. A panel that rendered both in the same green would
 * destroy the only signal the request calls "the loudest alert in this request".
 */
export type AirgapBanner = {
  /** `true` only while the gap is on. */
  active: boolean;
  /** `blocked`, `failed` or `clear`. */
  tone: string;
  /** The sentence. Empty when `active` is false. */
  message: string;
};

/** `GET /api/v1/ai/airgap` — one call answers every question this screen has. */
export type AirgapOverview = {
  state: AirgapState;
  would_block: BlockedProvider[];
  hosts: AirgapHost[];
  banner: AirgapBanner;
  /** What still answers while the gap is on. Empty while it is off, because then it says nothing. */
  still_available: string[];
};

/** The reason bounds, mirrored from `omnion_ai_hub::airgap_store`. */
export const REASON_MIN = 10;
export const REASON_MAX = 500;

/**
 * The whole switch state in one call.
 *
 * Reading it needs `ai.local.read`, not `ai.airgap.manage`: the read answers the locality badges'
 * question, and an operator who can see the endpoints must be able to see whether the gap is on.
 */
export function fetchAirgap(): Promise<AirgapOverview> {
  return request<AirgapOverview>("/api/v1/ai/airgap");
}

/**
 * Flip the switch.
 *
 * A reason is required on the way ON and ignored on the way OFF — the asymmetry is the API's, and
 * it is deliberate: turning the gap off is the emergency action, and an emergency action blocked
 * by a validation rule fails closed at the worst moment. `acknowledged` must be `true` when
 * providers would block, or the API answers `airgap_ack_required` **before** the write, so the
 * switch can never reach a state the operator did not confirm.
 */
export function setAirgap(body: {
  enabled: boolean;
  reason?: string | null;
  acknowledged?: boolean;
}): Promise<AirgapOverview> {
  return request<AirgapOverview>("/api/v1/ai/airgap", {
    method: "PUT",
    body: JSON.stringify({
      enabled: body.enabled,
      reason: body.reason ?? null,
      acknowledged: body.acknowledged ?? false,
    }),
  });
}

/**
 * Add an internal host the locality check may treat as local.
 *
 * The host must be bare — no scheme, no port, no path — because a pasted URL is the common
 * mistake and it would widen the check by nothing while appearing to succeed.
 */
export function addAirgapHost(body: { host: string; note?: string | null }): Promise<{
  host: string;
  note?: string | null;
  /** `true` when the row already existed, i.e. this was the same request submitted twice. */
  unchanged?: boolean;
}> {
  return request("/api/v1/ai/airgap/hosts", {
    method: "POST",
    body: JSON.stringify({ host: body.host, note: body.note ?? null }),
  });
}

/** Remove one allow-list row. The removal is what proves the list ever did anything. */
export function removeAirgapHost(id: string): Promise<void> {
  return request<void>(`/api/v1/ai/airgap/hosts/${encodeURIComponent(id)}`, {
    method: "DELETE",
  });
}

/**
 * Egress verification: attempt a non-local call and expect the refusal.
 *
 * # `outcome` names the MEASUREMENT, not the verdict
 *
 * A refusal is the **pass** here, so the field reads `blocked` (the call was blocked) rather
 * than `passed` (a judgement about the installation). The distinction is not pedantry: `blocked`
 * is a fact about one attempt and stays true even if the switch is broken somewhere else, while
 * `passed` would assert the whole control is sound from a single call — and a screen rendering
 * `passed` next to a red banner would be claiming two contradictory things at once.
 *
 * `undetermined` is the third value and it is NOT a pass: the attempt never reached the check,
 * so it proved nothing. Treating it as `passed` is the false reassurance the request warns about.
 *
 * `holds` is what the panel branches on; `outcome` is what a human reads. Both are returned so
 * the screen never has to re-derive the inversion — a client copy of the rule could disagree with
 * the checker on the one case that matters.
 */
export function verifyAirgap(target?: string | null): Promise<{
  enabled: boolean;
  /** `blocked` (the pass), `escaped` (a breach) or `undetermined` (proved nothing). */
  outcome: EgressOutcome;
  /** The convenience of `outcome === "blocked"`, computed server-side. */
  holds: boolean;
  /** The bare host that was aimed at. */
  target: string | null;
  /** The provider whose stored base URL was used. */
  provider: string | null;
  latency_ms: number | null;
  /** Unix seconds, or `null` when the attempt never completed. */
  verified_at: number | null;
  /** One plain-language sentence naming the outcome and the host. */
  message: string;
  /** The refusal itself, when there was one — the operator sees what they would have hit. */
  refusal: {
    code: string;
    message: string;
    provider: string;
    host: string;
  } | null;
}> {
  return request("/api/v1/ai/airgap/verify", {
    method: "POST",
    body: JSON.stringify({ target: target ?? null }),
  });
}

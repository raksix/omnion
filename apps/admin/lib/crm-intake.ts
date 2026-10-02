/**
 * The CRM lead inbox's own vocabulary and the small derivations the screens need
 * (docs/requests/REQ-117, slice 1).
 *
 * The types are declared here rather than in `types.ts` because they are the *panel's* view of
 * a module, not a shared primitive: one API surface, three screens, no other consumer. What
 * lives here is the mapping from the server's closed lists to the words an operator reads, and
 * the two or three derivations the API deliberately leaves to the client (a countdown, a
 * relative instant, a status tone).
 *
 * The server's lists stay the source of truth: the panels render whatever the answer says and
 * fall back to the raw value, so a server that grows a new status shows it instead of hiding it.
 */

/**
 * The status vocabulary the inbox renders; mirrors the module's `STATUSES`.
 *
 * This array and the Rust `STATUSES` are the same list written twice -- a
 * language boundary cannot import, and "fetch the vocabulary from the server"
 * would mean a screen that cannot render before its first response. The crate
 * already reads its own list against the migration for exactly this reason
 * (`the_migration_agrees_with_the_lists`); `the_panel_agrees_with_the_crate`
 * extends that check to a THIRD copy, this one, because the two directions fail
 * quietly and in opposite ways: a status the server allows and this list omits
 * is a lead whose pill renders raw `snake_case` (a value the platform accepts,
 * displayed as though the panel had never heard of it), and a status this panel
 * offers as a filter that the database refuses is a chip that quietly returns
 * nothing for ever. Neither raises anything, which is why the check has to read
 * the file rather than trust the types.
 */
export const LEAD_STATUSES = [
  "new",
  "assigned",
  "contacted",
  "qualified",
  "converted",
  "duplicate",
  "spam",
  "rejected",
] as const;

/** A lead's status: the closed list, or a value a newer server added to it. */
export type LeadStatus = (typeof LEAD_STATUSES)[number];

/** The dedupe policies a source can carry. */
export const DEDUPE_POLICIES = ["link", "create_anyway", "reject_duplicate"] as const;

/** What a source can be: a form, a keyed endpoint, or a manual import. */
export const SOURCE_KINDS = ["form", "endpoint", "import"] as const;

/**
 * `true` when a lead in this status is still work -- the panel's half of the
 * module's `is_open`, and the answer the SLA clock, the "Open" counter and the
 * status filter chips all read.
 *
 * It was exported and never called for the whole life of the module while its
 * complement, `CLOSED_LEAD_STATUSES`, was used in two places. That is the exact
 * shape this branch has now paid for fourteen times -- a correct, documented,
 * exported answer with no caller -- and the reason the filter chips read the
 * complement twice (`closed ? muted : ink`) is that the code that needed the
 * *open* half inlined the inverse rather than ask. Deriving both sets from the
 * single `LEAD_STATUSES` array is what makes that impossible: a status added to
 * the list lands in exactly one of them, and the panel cannot render a chip it
 * has not classified.
 */
export function isOpenLeadStatus(status: string): boolean {
  return !CLOSED_LEAD_STATUSES.has(status);
}

/**
 * Statuses that are a file, not a task: the inbox hides them behind a filter.
 *
 * The panel's half of the module's partition -- `is_open` there, this set here.
 * The two are checked against each other rather than derived from one another
 * because they live in different languages, and a comment claiming they are
 * derived is exactly the kind of claim that hides the next status added to one
 * side only.
 */
export const CLOSED_LEAD_STATUSES: ReadonlySet<string> = new Set<string>([
  "converted",
  "duplicate",
  "spam",
  "rejected",
]);

/** The words a status is read as, rather than the stored snake_case value. */
export const LEAD_STATUS_LABEL: Record<string, string> = {
  new: "New",
  assigned: "Assigned",
  contacted: "Contacted",
  qualified: "Qualified",
  converted: "Converted",
  duplicate: "Duplicate",
  spam: "Spam",
  rejected: "Rejected",
};

/** The tone of a status pill. Closed verdicts are quiet; work is the accent. */
export const LEAD_STATUS_TONE: Record<string, string> = {
  new: "bg-accent-soft text-accent-strong",
  assigned: "bg-caution-soft text-caution",
  contacted: "bg-caution-soft text-caution",
  qualified: "bg-positive-soft text-positive",
  converted: "bg-positive-soft text-positive",
  duplicate: "bg-quiet-soft text-muted",
  spam: "bg-quiet-soft text-muted",
  rejected: "bg-quiet-soft text-muted",
};

/** The words a dedupe policy is read as. */
export const DEDUPE_POLICY_LABEL: Record<string, string> = {
  link: "Link to the matching contact",
  create_anyway: "Always create a new contact",
  reject_duplicate: "File as a duplicate",
};

/** The words a source kind is read as. */
export const SOURCE_KIND_LABEL: Record<string, string> = {
  form: "Form",
  endpoint: "Keyed endpoint",
  import: "Manual import",
};

/**
 * The first-response clock's state, as the **server** answers it.
 *
 * This used to be a second implementation. It derived the same four states from the deadline,
 * the instant it was met and a hard-coded `AT_RISK_MINUTES = 60`, and its own doc said slice 2
 * would "take over this function rather than the screens, so nothing here has to change" —
 * slice 2 landed and nothing changed, so for three ticks the panel and the module disagreed by
 * construction. The module warns at a quarter of the **policy's own window**, floored at 15
 * minutes, so on a 15-minute policy a lead with 5 minutes left is `at_risk` where this called
 * it `on_track`; and a lead answered *after* its deadline is `met` in the module while this
 * showed `breached` for ever.
 *
 * It is now a pass-through, and that is the point: the only rule left here is the *shape* of
 * the answer, and the badge reads whatever the server said. There is deliberately no fallback
 * arithmetic — a client that "helpfully" recomputes when the field is missing is the second
 * implementation this change exists to delete.
 */
export type SlaState = "on_track" | "at_risk" | "breached" | "met" | "none";

/** The words each clock state is read as. */
export const SLA_STATE_LABEL: Record<SlaState, string> = {
  on_track: "On track",
  at_risk: "Due soon",
  breached: "Breached",
  met: "Responded",
  none: "No target",
};

/** The tone of each clock state; a breach is the only red thing on the screen. */
export const SLA_STATE_TONE: Record<SlaState, string> = {
  on_track: "bg-quiet-soft text-muted",
  at_risk: "bg-caution-soft text-caution",
  breached: "bg-red-500/10 text-red-700",
  met: "bg-positive-soft text-positive",
  none: "bg-quiet-soft text-muted",
};

/**
 * The clock state of one lead, read from the server's answer.
 *
 * `sla_state` is `none` when the lead has no SLA policy attached, and the panel renders that
 * as "No target" rather than a green `On track` — a badge claiming a promise nobody made is
 * the failure this replaced, so it is not reintroduced as a default. An unrecognised word
 * degrades to `none` rather than throwing: a state the server adds later must render as
 * "nothing to say", never as a blank screen or a crash.
 */
export function slaState(lead: { sla_state?: string }): SlaState {
  const value = lead.sla_state;
  return value === "on_track" || value === "at_risk" || value === "breached" || value === "met"
    ? value
    : "none";
}

/** A countdown in words — "in 2h 15m", "2h 04m late" — or `null` without a deadline. */
export function countdown(dueAt: string | null, now = Date.now()): string | null {
  if (!dueAt) {
    return null;
  }
  const due = Date.parse(dueAt);
  if (!Number.isFinite(due)) {
    return null;
  }
  const minutes = Math.round(Math.abs(due - now) / 60_000);
  if (minutes < 1) {
    return due < now ? "just overdue" : "due now";
  }
  const days = Math.floor(minutes / 1440);
  const hours = Math.floor((minutes % 1440) / 60);
  const rest = minutes % 60;
  const body =
    days > 0
      ? `${days}d ${hours}h`
      : hours > 0
        ? `${hours}h ${String(rest).padStart(2, "0")}m`
        : `${minutes}m`;
  return due < now ? `${body} late` : `in ${body}`;
}

/**
 * An absolute instant, or an honest placeholder.
 *
 * The server used to answer timestamps in Rust's `Display` spelling — `2026-10-01 15:32:24
 * … +00:00:00`, a space where RFC 3339 puts `T` — and every `new Date(...)` on that string is
 * `Invalid Date`. `toLocaleString()` then renders the *words* "Invalid Date", so the lead
 * screen said **"Responded Invalid Date"** on the one field that answers "did we get to them
 * in time?". The server now emits RFC 3339, and this function is the second half of that fix:
 * a formatter that says "unknown" is the behaviour a screen should have had all along, and
 * the two together mean a future bad timestamp shows an em dash rather than a sentence that
 * looks like data.
 *
 * It is one function rather than three `new Date()` call sites for the same reason the server
 * side is one function: the inbox already had a tolerant reader (`relativeInstant`) and the
 * detail screen had an intolerant one, and two readers of the same field is how a screen
 * ends up disagreeing with itself about whether a date exists.
 */
export function absoluteInstant(value: string | null | undefined): string {
  if (!value) {
    return "unknown";
  }
  const at = new Date(value);
  return Number.isNaN(at.getTime()) ? "unknown" : at.toLocaleString();
}

/** A relative instant — "just now", "14m ago", "3d ago" — for the inbox's Received column. */
export function relativeInstant(value: string | null, now = Date.now()): string {
  if (!value) {
    return "—";
  }
  const at = Date.parse(value);
  if (!Number.isFinite(at)) {
    return "—";
  }
  const minutes = Math.round((now - at) / 60_000);
  if (minutes < 1) {
    return "just now";
  }
  if (minutes < 60) {
    return `${minutes}m ago`;
  }
  const hours = Math.round(minutes / 60);
  if (hours < 24) {
    return `${hours}h ago`;
  }
  const days = Math.round(hours / 24);
  return days < 30 ? `${days}d ago` : new Date(at).toLocaleDateString();
}

/** The person's name as one string, with the e-mail beside it when there is no name. */
export function contactLabel(lead: {
  first_name: string | null;
  last_name: string | null;
  email: string | null;
  phone: string | null;
}): string {
  const name = [lead.first_name, lead.last_name].filter(Boolean).join(" ").trim();
  if (name) {
    return name;
  }
  if (lead.email) {
    return lead.email;
  }
  return lead.phone ?? "Unnamed submission";
}

/** The mapping targets the source editor offers, with the panel's own words. */
export const MAPPING_TARGET_LABEL: Record<string, string> = {
  first_name: "First name",
  last_name: "Last name",
  email: "E-mail",
  phone: "Phone",
  company_name: "Company name",
  job_title: "Job title",
  message: "Message / notes",
  product_interest: "Product interest",
  country: "Country",
  region: "Region",
  language: "Language",
  budget_band: "Budget band",
  quantity: "Quantity",
  preferred_contact_time: "Preferred contact time",
};

/** The transforms a mapping line may carry, with the panel's own words. */
export const MAPPING_TRANSFORM_LABEL: Record<string, string> = {
  trim: "Trim",
  lowercase: "Lowercase",
  title_case: "Title case",
  strip_html: "Strip HTML",
  e164_lite: "Phone (E.164-lite)",
  split_full_name: "Split full name",
};

/** The CRM fields a lead row can be built from — the same list the API validates against. */
export const MAPPING_TARGETS = Object.keys(MAPPING_TARGET_LABEL);

/** The transforms the panel offers, in the order they are applied. */
export const MAPPING_TRANSFORMS = Object.keys(MAPPING_TRANSFORM_LABEL);

/**
 * A lead's trail, rendered as one line. Unknown kinds keep their raw name.
 *
 * `assigned` and `reassigned` are two kinds on purpose — the store reads the choice off the
 * row's previous owner rather than off whether the id changed — so the words are two as well,
 * and a panel that rendered both as "Assigned" would throw away the distinction the trail was
 * built to keep.
 */
export const LEAD_EVENT_LABEL: Record<string, string> = {
  received: "Received",
  status_changed: "Status changed",
  edited: "Edited",
  responded: "First response recorded",
  assigned: "Assigned",
  reassigned: "Owner changed",
  converted: "Converted",
  conversion_skipped: "Conversion skipped",
  autoresponder_sent: "Autoresponder",
};

/**
 * What an autoresponder claim *is*, in an operator's words.
 *
 * `abandoned` is the state the panel could not have worked out on its own: the claim was taken,
 * the caller never recorded a delivery, and the row is old enough that the worker may take it
 * over. Before this was a named state the timeline showed it as a plain reservation — the same
 * rendering as a claim still in flight — on a lead whose only reply was never sent.
 */
export type AutoresponderState =
  | "sent"
  | "reserved"
  | "claimed"
  | "abandoned"
  | "unknown";

/**
 * The words for each state, exhaustive over the type above.
 *
 * **Closed on purpose, with `satisfies` rather than `Record<string, string>`.** A state the
 * server adds but this map does not carry is a type error here, rather than a lookup that
 * answers `undefined` and renders nothing — which is the failure mode of `LEAD_EVENT_LABEL`
 * above, and the reason the kinds map stays open (kinds are a growing vocabulary) while this one
 * does not (a claim is in one of exactly these five states).
 */
export const AUTORESPONDER_STATE_LABEL = {
  sent: "Reply sent",
  reserved: "Reply reserved, waiting out the send delay",
  claimed: "Claimed for sending",
  abandoned: "Claimed, but no delivery was recorded",
  unknown: "Answered before deliveries were recorded",
} satisfies Record<AutoresponderState, string>;

/**
 * The words an autoresponder *skip* note is read as.
 *
 * ## The gap this closes, and why it is not the map above's twin
 *
 * `ClaimState::of` answers `None` for a skip note — deliberately, because a skip line carries
 * no `sent` key and the panel must not render a claim chip for a row that never claimed
 * anything. That decision is right, and it left the line with **no words at all**: the
 * timeline rendered `event.detail.reason` verbatim, and every value in it is a machine word
 * the server invented — `not_accepted`, `no_address`, `invalid_template`, `source_disabled`,
 * `delayed`, `not_configured`, `already_sent`. An operator opening a lead to ask "why did
 * nobody email this person?" read `not_accepted`, which is a token, not a sentence.
 *
 * **This map is keyed on the same word `ClaimState` is keyed on**, and the two maps answer
 * different questions on purpose: the state says what happened to a *message*, this says why a
 * *decision* went the way it did. A skip has no message, so there is no state; without this the
 * line falls through to raw text, and a line with no owner is a line the platform renders in
 * whatever the server happened to call it.
 *
 * Open, not `satisfies`, and deliberately the opposite of `AUTORESPONDER_STATE_LABEL` above:
 * these words are a *growing* vocabulary — every `Delivery` variant has a `reason()`, and the
 * server may add one — so an unknown word must fall through to the raw value rather than
 * become a type error at a distance from the variant that caused it. `skipLabel` is where that
 * fallback is written, once.
 */
export const AUTORESPONDER_SKIP_LABEL: Record<string, string> = {
  not_configured: "This source has no autoresponder, so nothing was sent",
  not_accepted: "The submission was never accepted (spam, rejected or a duplicate)",
  no_address: "The mapping produced no e-mail address for this lead",
  invalid_template: "The autoresponder has a subject or body that renders to nothing",
  delayed: "Reserved — it is waiting out the configured send delay",
  already_sent: "This lead had already been answered",
  source_disabled: "The autoresponder was switched off while this reply was waiting",
};

/**
 * A skip note's sentence, falling through to the raw word when the server invents a new one.
 *
 * The fallback is the point and not a placeholder: an unmapped word is *better* shown raw
 * than shown as nothing, because a human can read `brand_new_verdict` and infer it is a case
 * the panel has not been taught, whereas a blank line reads as "no reason recorded".
 */
export function skipLabel(reason: string): string {
  return AUTORESPONDER_SKIP_LABEL[reason] ?? reason;
}

/**
 * How an owner reads in a row: their name when the roster knows them, and a short id when it
 * does not.
 *
 * The fallback is deliberate rather than a dash. A deleted or platform account still owns a
 * lead until somebody reassigns it, and a screen that renders nothing for it reads as "nobody
 * owns this" — which is the one thing an audit screen must never be wrong about. A short id is
 * findable; a blank is not.
 */
export function ownerLabel(owners: Map<string, string>, userId: string | null): string {
  if (!userId) return "Unassigned";
  return owners.get(userId) ?? `Former member ${userId.slice(0, 8)}`;
}

/** The words a dedupe decision is read as. */
export const DECISION_LABEL: Record<string, string> = {
  created: "New person",
  linked: "Linked to a contact",
  duplicate: "Duplicate of a contact",
  rejected: "Rejected",
  spam: "Filed as spam",
};

/** What a refresh does to the open source editor. */
export type EditorAfterRefresh<T> =
  /** The editor was closed, or stays open on the state the operator is holding. */
  | { action: "keep"; editing: T | null }
  /** The source the editor was editing is gone from the list, so the editor closes. */
  | { action: "close" };

/**
 * What a list refresh must do to the open source editor.
 *
 * **Keep, do not rehydrate.** An editor that follows the server here throws away a half-typed
 * mapping, a renamed source and a consent wording the operator is still writing, and it does
 * it on the one gesture that feels harmless — `Refresh`, which is a *read*. The old code said
 * in a comment that it must not do that and then did it: on an open editor it replaced the
 * state with the freshly fetched row, and on a closed one it returned `null`, which is
 * precisely the inverse of the stated intent.
 *
 * So the two arms are separated rather than merged into one expression: an editor that is
 * closed has nothing to protect, and one that is open is only closed when the source it is
 * editing is **no longer in the list** — deleted by somebody else, or deleted by this
 * operator on another tab. That is the single case where keeping the draft would leave an
 * editor writing to a row that is not there.
 *
 * `T` stays the caller's own state type and only `id` is required, so the decision is a pure
 * function of two values and can be driven directly: the rule that decides whether an
 * operator's in-flight work survives is not something to be reasoned about from a JSX
 * callback every time it is read.
 */
export function editorAfterRefresh<T extends { id: string }>(
  editing: T | null,
  sources: readonly { id: string }[],
): EditorAfterRefresh<T> {
  if (editing === null) {
    return { action: "keep", editing: null };
  }
  return sources.some((source) => source.id === editing.id)
    ? { action: "keep", editing }
    : { action: "close" };
}

/**
 * How the inbox's **Source** column reads a lead, and what it says when it cannot.
 *
 * **The column the REQ names and the table never had.** The request's inbox row has listed
 * `Source` since it was written; the filter above the table has carried the word since the same
 * tick, so the two reads were already in the product — and neither was in the row, which means
 * an operator who filtered by one source and then read the table could not tell what a row in
 * front of them was without going back to the filter. The value is derived from the source
 * roster the screen already holds, not from a join: the lead carries `source_id`, the panel
 * already reads every source for the filter, and a join would make the inbox's page query
 * depend on a second table for one cell of text.
 *
 * **A lead whose source is not in the roster is named by its short id rather than by a
 * dash**, for the same reason `ownerLabel` does it: a source deleted behind a lead, or one
 * another tab created after this page loaded, is a real row with a real cause, and a blank
 * cell reads as "no source" — which sends the operator to the form side instead of the
 * settings side, where the answer is.
 */
export function sourceLabel(
  sources: Map<string, string>,
  sourceId: string | null | undefined,
): string | null {
  if (!sourceId) {
    // **No source at all is the one case with a blank cell.** An imported lead has no
    // capture surface and `source_id` is null for a reason that is not a fault, so the
    // honest cell is empty rather than a fabricated "Imported" the screen cannot back.
    return null;
  }
  return sources.get(sourceId) ?? `Source ${sourceId.slice(0, 8)}`;
}

/**
 * The **Duplicate hint** cell, and the rule that decides when it says anything.
 *
 * The inbox's column list has said `Duplicate hint` since the request was written and the
 * duplicates queue has rendered the matched key and the score for a long time — but on
 * `/crm/leads/duplicates`, one screen further along. An operator asking "is this the same
 * person who wrote to me last week?" is standing on the **inbox**, and the only answer they
 * could get was to filter by the `duplicate` status and walk to the queue.
 *
 * **Only a row that matched somebody shows a hint.** `decision = "linked"` is not a
 * duplicate — it is the *good* outcome of the dedupe pass, the lead that now belongs to an
 * existing contact — and a hint that fires on it tells an operator twenty ordinary leads are
 * suspicious. So the cell reads the two facts together: a stored decision that means "filed as
 * a duplicate", and the matched key beside it so the claim is checkable rather than an
 * accusation.
 */
export type DuplicateHint = {
  /**
   * **Which of the two columns carried the verdict.**
   *
   * Recorded rather than collapsed into a boolean because the two answer different
   * questions and an operator reads the difference: `verdict` is the dedupe pass's own
   * decision ("matched somebody, filed, not linked"), while `status` is the row's terminal
   * state — which is also how a `keep separate` decision in the duplicates queue leaves a
   * row that never had a verdict recorded against it. Rendering both as the word
   * "duplicate" would make a queue decision indistinguishable from a dedupe match.
   */
  from: "verdict" | "status";
  /** The key the verdict matched on, when one was recorded. */
  key: string | null;
  /** The confidence, 0-1, when one was recorded. */
  score: number | null;
};

/** Whether a lead's stored verdict means "this is somebody we already have". */
export function isDuplicateVerdict(lead: {
  decision?: string | null;
  status?: string | null;
}): boolean {
  // Two columns carry this fact and they are not redundant. `status = "duplicate"` is the
  // row's *terminal state* — the filter chip and every terminal-status gate read it — while
  // `decision = "duplicate"` is the *dedupe pass's verdict*, which a `reject_duplicate`
  // source files and a `keep separate` decision in the queue later clears. Reading only one
  // of them produces a column that is right for one policy and silent for the other.
  return lead.status === "duplicate" || lead.decision === "duplicate";
}

/** What the Duplicate hint cell renders, or `null` when the row is not a duplicate. */
export function duplicateHint(lead: {
  decision?: string | null;
  dedupe_key?: string | null;
  dedupe_score?: number | null;
  status?: string | null;
}): DuplicateHint | null {
  if (lead.decision === "duplicate") {
    return {
      from: "verdict",
      key: lead.dedupe_key ?? null,
      score: lead.dedupe_score ?? null,
    };
  }
  // The status arm is the row whose verdict was never recorded — a duplicate filed by an
  // older source, or one whose `decision` was cleared by a `keep separate` that left the
  // terminal status behind. Both are still duplicates to a reader, and the column says so
  // rather than waiting for a dedupe pass to run again.
  if (lead.status === "duplicate") {
    return {
      from: "status",
      key: lead.dedupe_key ?? null,
      score: lead.dedupe_score ?? null,
    };
  }
  return null;
}

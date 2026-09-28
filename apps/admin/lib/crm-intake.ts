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

/** The status vocabulary the inbox renders; mirrors the module's `STATUSES`. */
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

/** The dedupe policies a source can carry. */
export const DEDUPE_POLICIES = ["link", "create_anyway", "reject_duplicate"] as const;

/** What a source can be: a form, a keyed endpoint, or a manual import. */
export const SOURCE_KINDS = ["form", "endpoint", "import"] as const;

/** The statuses an operator still works — a lead outside this set is a filed verdict. */
export const OPEN_LEAD_STATUSES = new Set<string>([
  "new",
  "assigned",
  "contacted",
  "qualified",
]);

/** Statuses that are a file, not a task: the inbox hides them behind a filter. */
export const CLOSED_LEAD_STATUSES = new Set<string>(["converted", "duplicate", "spam", "rejected"]);

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
 * The first-response clock's state, derived from three fields the API sends.
 *
 * The platform's SLA policies are slice 2, so there is no `sla_state` column to read: this
 * derives the same four states from the deadline, the instant it was met and whether the clock
 * is still running. When slice 2 lands it takes over this function rather than the screens, so
 * nothing here has to change.
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

/** How close to the deadline counts as "due soon". */
const AT_RISK_MINUTES = 60;

/**
 * The clock state of one lead.
 *
 * `dueAt` null means the source has no SLA policy attached yet (slice 2 attaches one), and the
 * honest answer there is "No target" rather than a green `On track` that claims a promise
 * nothing made.
 */
export function slaState(lead: {
  first_response_due_at: string | null;
  first_response_at: string | null;
  is_open: boolean;
  status: string;
}): SlaState {
  if (lead.first_response_at) {
    // Responding after the deadline keeps the breach recorded: the deadline passed, and the
    // report that measures the breach has to keep seeing it.
    const due = lead.first_response_due_at ? Date.parse(lead.first_response_due_at) : null;
    const met = Date.parse(lead.first_response_at);
    if (due !== null && met > due) {
      return "breached";
    }
    return "met";
  }
  if (!lead.is_open || CLOSED_LEAD_STATUSES.has(lead.status)) {
    return "none";
  }
  if (!lead.first_response_due_at) {
    return "none";
  }
  const due = Date.parse(lead.first_response_due_at);
  if (!Number.isFinite(due)) {
    return "none";
  }
  if (due < Date.now()) {
    return "breached";
  }
  if (due - Date.now() <= AT_RISK_MINUTES * 60_000) {
    return "at_risk";
  }
  return "on_track";
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

/** A lead's trail, rendered as one line. Unknown kinds keep their raw name. */
export const LEAD_EVENT_LABEL: Record<string, string> = {
  received: "Received",
  status_changed: "Status changed",
  edited: "Edited",
  responded: "First response recorded",
};

/** The words a dedupe decision is read as. */
export const DECISION_LABEL: Record<string, string> = {
  created: "New person",
  linked: "Linked to a contact",
  duplicate: "Duplicate of a contact",
  rejected: "Rejected",
  spam: "Filed as spam",
};

/**
 * `/crm/settings/assignment` — the ordered rule chain and the simulator (REQ-117, slice 2).
 *
 * The screen exists to answer two questions an operator actually has, and its whole design
 * follows from taking them seriously:
 *
 * 1. **"Who gets the next lead?"** The table shows each rule's target, and a pool rule shows
 *    the person the *next* matching lead would go to. That number comes from the server
 *    (`next_pool_user_id`) and reading it costs no fairness — the simulator deliberately does
 *    not advance the cursor, so opening this page never changes who answers what.
 * 2. **"Why didn't my other rule win?"** A preview that only names the winner cannot answer
 *    that, so the simulator also lists every rule it passed over *with the condition key that
 *    missed*. That is the difference between a settings screen and a black box.
 *
 * Two rules the rest of the file is built around:
 *
 * * **The order on screen is the order the server evaluates.** The panel never sorts a copy:
 *    move-up and move-down send the reordered prefix to `PUT /crm/assignment/rules/order` and
 *    re-render from the answer, because the server's renumbering is the only definition of
 *    "second from the top".
 * * **A rule that cannot win says so.** A `queue` rule and a pool whose members were all
 *    deleted both render as "matches everything, assigns to nobody", which is the truth and
 *    not an error state — the chain is allowed to end in the visible unassigned queue.
 */

"use client";

import { useCallback, useEffect, useMemo, useState } from "react";

import { ApiError, request } from "@/lib/api";

// -------------------------------------------------------------------------------------------
// Types
// -------------------------------------------------------------------------------------------

/** A rule as `/crm/assignment/rules` answers it. */
export interface AssignmentRule {
  id: string;
  organization_id: string;
  name: string;
  position: number;
  conditions: Record<string, unknown>;
  target_kind: "user" | "pool" | "queue";
  target_user_id: string | null;
  pool_user_ids: string[];
  round_robin_cursor: number;
  active: boolean;
  condition_summary: string;
  candidate_count: number;
  next_pool_user_id: string | null;
}

/** One rule the evaluator passed over, and the key that made it miss. */
interface SkippedRule {
  rule_id: string;
  rule_name: string;
  failed_on: string;
}

interface SimulateOutcome {
  rule_id: string | null;
  rule_name: string | null;
  target_kind: string | null;
  owner_user_id: string | null;
  matched_index: number | null;
  skipped: SkippedRule[];
  cursor_before: number | null;
  cursor_after: number | null;
}

interface SimulateAnswer {
  outcome: SimulateOutcome;
  input_read: Record<string, string | boolean | null>;
  /** Keys the paste carried that no rule can see. Empty is the healthy answer. */
  unread: UnreadKey[];
  readable_keys: ReadableKey[];
  rules_considered: number;
  wrote_nothing: boolean;
}

/** A key the paste carried that no condition reads, and the alias it probably meant. */
interface UnreadKey {
  key: string;
  did_you_mean: string | null;
}

/**
 * What the payload reader accepts, per condition key, as the server publishes it.
 *
 * The list is **fetched, not written here**, for the same reason the target picker is: the
 * reader and the validator must be the same table, and a fourth copy in this file would be
 * the one that drifts — the reader would gain an alias and the panel's hint would not.
 */
interface ReadableKey {
  condition: string;
  aliases: string[];
}

/** The closed list the database enforces, fetched rather than hard-coded. */
type Targets = string[];

/** The three ways a rule can be read, in the words the editor uses. */
const TARGET_LABEL: Record<string, string> = {
  user: "one person",
  pool: "round-robin pool",
  queue: "the unassigned queue",
};

/**
 * The condition keys a lead carries, with the label the chips render. Fetching the closed
 * *targets* list from the server is what keeps this honest: the database refuses anything
 * outside it, so a picker offering `round_robin` is a settings screen whose last option is a
 * lie. The condition keys are the crate's list and are rendered from the same spellings the
 * validator accepts, so a typo in this table shows up as a refusal the operator can read.
 */
const CONDITION_KEYS: { key: string; label: string; hint: string }[] = [
  { key: "country", label: "Country", hint: "e.g. TR" },
  { key: "region", label: "Region", hint: "e.g. Marmara" },
  { key: "product_interest", label: "Product interest", hint: "e.g. Analytics" },
  { key: "budget_band", label: "Budget band", hint: "e.g. enterprise" },
  { key: "source_name", label: "Source name", hint: "e.g. Quote form" },
  { key: "language", label: "Language", hint: "e.g. tr" },
];

// -------------------------------------------------------------------------------------------
// The screen
// -------------------------------------------------------------------------------------------

export function AssignmentSettings() {
  // The live editor's form, lifted so the simulator can evaluate an unsaved rule against the
  // saved chain. Kept separate from `editing` on purpose: `editing` is *which* rule is open
  // (a saved row, or "new"), and the thing the simulator needs is the form as it stands right
  // now — the two differ on every keystroke, and reading the saved row would answer about a
  // rule the operator has already changed.
  const [draftPreview, setDraftPreview] = useState<RuleDraft | null>(null);

  const [rules, setRules] = useState<AssignmentRule[] | null>(null);
  const [targets, setTargets] = useState<Targets | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [editing, setEditing] = useState<AssignmentRule | "new" | null>(null);

  const load = useCallback(async () => {
    try {
      const [list, kinds] = await Promise.all([
        request<AssignmentRule[]>("/api/v1/crm/assignment/rules"),
        request<Targets>("/api/v1/crm/assignment/targets"),
      ]);
      setRules(list);
      setTargets(kinds);
      setError(null);
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : String(caught));
      setRules([]);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  /** Move a rule one place and let the server renumber the whole chain. */
  const move = async (index: number, delta: number) => {
    if (!rules) return;
    const next = index + delta;
    if (next < 0 || next >= rules.length) return;
    const order = [...rules];
    const [moved] = order.splice(index, 1);
    order.splice(next, 0, moved);
    await run(
      () =>
        request<AssignmentRule[]>("/api/v1/crm/assignment/rules/order", {
          method: "PUT",
          body: JSON.stringify({ ids: [moved.id] }),
        }),
      order,
    );
  };

  const remove = async (rule: AssignmentRule) => {
    await run(
      () => request<null>(`/api/v1/crm/assignment/rules/${rule.id}`, { method: "DELETE" }),
    );
  };

  const toggle = async (rule: AssignmentRule) => {
    await run(
      () =>
        request<AssignmentRule>(`/api/v1/crm/assignment/rules/${rule.id}`, {
          method: "PATCH",
          body: JSON.stringify({
            name: rule.name,
            conditions: rule.conditions,
            target_kind: rule.target_kind,
            target_user_id: rule.target_user_id,
            pool_user_ids: rule.pool_user_ids,
            active: !rule.active,
          }),
        }),
    );
  };

  const save = async (draft: RuleDraft) => {
    const body = JSON.stringify(draft);
    await run(() =>
      draft.id
        ? request<AssignmentRule>(`/api/v1/crm/assignment/rules/${draft.id}`, {
            method: "PATCH",
            body,
          })
        : request<AssignmentRule>("/api/v1/crm/assignment/rules", {
            method: "POST",
            body,
          }),
    );
  };

  /** Every mutating action goes through here: one spinner, one refresh, one error line. */
  const run = async (action: () => Promise<unknown>, optimistic?: AssignmentRule[]) => {
    setBusy(true);
    if (optimistic) setRules(optimistic);
    try {
      await action();
      await load();
      setEditing(null);
    } catch (caught) {
      setError(caught instanceof ApiError ? caught.message : String(caught));
      // Re-read rather than guess: an optimistic order that the server refused must not
      // stay on screen, and the server's answer is the truth about what the chain is.
      await load();
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-6">
      <header className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h2 className="text-[15px] font-medium text-ink">Assignment rules</h2>
          <p className="mt-0.5 max-w-prose text-[12.5px] text-muted">
            Evaluated top-down; the first rule that matches wins. A lead that matches nothing
            lands in the unassigned queue below, which is the same thing as the last rule
            choosing the queue on purpose.
          </p>
        </div>
        <button
          type="button"
          disabled={busy || rules === null}
          onClick={() => setEditing("new")}
          data-testid="assignment-add"
          className="rounded-md bg-ink px-3 py-1.5 text-[12.5px] text-paper disabled:opacity-50"
        >
          Add rule
        </button>
      </header>

      {error ? (
        <p
          role="alert"
          data-testid="assignment-error"
          className="rounded-md border border-danger/40 bg-danger/5 px-3 py-2 text-[12.5px] text-danger"
        >
          {error}
        </p>
      ) : null}

      {rules === null ? (
        <RulesSkeleton />
      ) : rules.length === 0 ? (
        <p className="rounded-md border border-line px-3 py-6 text-center text-[12.5px] text-muted">
          No rules yet. Without one, every lead waits in the unassigned queue.
        </p>
      ) : (
        <ol className="space-y-2" data-testid="assignment-rules">
          {rules.map((rule, index) => (
            <li
              key={rule.id}
              data-testid="assignment-rule"
              data-rule-id={rule.id}
              data-position={rule.position}
              className="flex flex-wrap items-center gap-3 rounded-md border border-line px-3 py-2.5"
            >
              <span
                aria-hidden
                className="w-5 shrink-0 text-center text-[11px] tabular-nums text-muted"
              >
                {index + 1}
              </span>
              <div className="min-w-0 flex-1">
                <p className="flex items-center gap-2 text-[13px] text-ink">
                  <span className="truncate font-medium">{rule.name}</span>
                  {rule.active ? null : (
                    <span
                      data-testid="rule-inactive"
                      className="rounded border border-line px-1.5 py-0.5 text-[10.5px] text-muted"
                    >
                      inactive — never evaluated
                    </span>
                  )}
                </p>
                <p className="mt-0.5 truncate text-[11.5px] text-muted" title={rule.condition_summary}>
                  {rule.condition_summary} →{" "}
                  {TARGET_LABEL[rule.target_kind] ?? rule.target_kind}
                  {rule.target_kind === "pool"
                    ? ` · next: ${short(rule.next_pool_user_id)}`
                    : ""}
                  {rule.target_kind === "user" && !rule.target_user_id
                    ? " · the person this rule names is gone"
                    : ""}
                </p>
              </div>
              <div className="flex shrink-0 items-center gap-1">
                <IconButton
                  label={`Move ${rule.name} up`}
                  disabled={busy || index === 0}
                  onClick={() => void move(index, -1)}
                  testId={`rule-up-${index}`}
                >
                  ↑
                </IconButton>
                <IconButton
                  label={`Move ${rule.name} down`}
                  disabled={busy || index === rules.length - 1}
                  onClick={() => void move(index, 1)}
                  testId={`rule-down-${index}`}
                >
                  ↓
                </IconButton>
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => void toggle(rule)}
                  data-testid={`rule-toggle-${index}`}
                  className="rounded border border-line px-2 py-1 text-[11.5px] text-ink disabled:opacity-50"
                >
                  {rule.active ? "Deactivate" : "Activate"}
                </button>
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => setEditing(rule)}
                  data-testid={`rule-edit-${index}`}
                  className="rounded border border-line px-2 py-1 text-[11.5px] text-ink disabled:opacity-50"
                >
                  Edit
                </button>
                <button
                  type="button"
                  disabled={busy}
                  onClick={() => void remove(rule)}
                  data-testid={`rule-delete-${index}`}
                  className="rounded border border-danger/40 px-2 py-1 text-[11.5px] text-danger disabled:opacity-50"
                >
                  Delete
                </button>
              </div>
            </li>
          ))}
        </ol>
      )}

      <Simulator targets={targets ?? []} draft={draftPreview} />

      {editing ? (
        <RuleEditor
          draft={editing === "new" ? null : editing}
          targets={targets ?? []}
          onPreview={setDraftPreview}
          onCancel={() => {
            setEditing(null);
            setDraftPreview(null);
          }}
          onSave={(draft) => void save(draft)}
        />
      ) : null}
    </div>
  );
}

/**
 * The editor's form as the simulator sends it.
 *
 * Two conversions happen here and both are the alternative to a server-side guess. `id` is
 * dropped because a draft is not a rule yet — the server gives it `Uuid::nil()` and names it
 * "(unsaved)", so sending one would suggest the server reads it. And a ticked-but-blank
 * condition is dropped rather than sent as an empty list, for the reason the editor's own
 * `submit` gives: an empty list matches nothing, so sending it would let the simulator
 * answer "your rule matches nothing" for a form the operator is still filling in.
 */
function draftToServer(draft: RuleDraft): Record<string, unknown> {
  const conditions: Record<string, string[]> = {};
  for (const { key } of CONDITION_KEYS) {
    const values = draft.conditions[key];
    if (values && values.length > 0) conditions[key] = values;
  }
  return {
    name: draft.name.trim() || "Untitled draft",
    conditions,
    target_kind: draft.target_kind,
    target_user_id: draft.target_kind === "user" ? draft.target_user_id.trim() || null : null,
    pool_user_ids: draft.target_kind === "pool" ? draft.pool_user_ids : [],
    active: draft.active,
  };
}

function short(id: string | null | undefined): string {
  if (!id) return "nobody yet";
  return `${id.slice(0, 8)}…`;
}

/** A borderless square button; the label is the accessible name and the tooltip. */
function IconButton({
  label,
  disabled,
  onClick,
  testId,
  children,
}: {
  label: string;
  disabled: boolean;
  onClick: () => void;
  testId: string;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      disabled={disabled}
      onClick={onClick}
      data-testid={testId}
      className="h-7 w-7 rounded border border-line text-[12px] text-ink disabled:opacity-40"
    >
      {children}
    </button>
  );
}

function RulesSkeleton() {
  return (
    <ul className="space-y-2" aria-busy="true" data-testid="assignment-skeleton">
      {[0, 1].map((n) => (
        <li key={n} className="h-[58px] animate-pulse rounded-md border border-line bg-line/30" />
      ))}
    </ul>
  );
}

// -------------------------------------------------------------------------------------------
// The simulator
// -------------------------------------------------------------------------------------------

/**
 * The simulator pastes a lead-shaped payload and answers which rule would win — and, just
 * as importantly, which rules would not and why.
 *
 * The "why not" list is the feature. An operator who adds a country rule and sees it not fire
 * needs to know *which* condition it was that did not match, and the server names it in
 * `failed_on`, so the screen shows that string rather than a generic "did not match".
 *
 * **Two questions, and the second one is the one a skip list cannot answer.** The skip list
 * names the *rule's* condition, which reads as "your rule is wrong". Usually the rule is
 * right and the *paste* is the problem: `contury` is a key the reader does not know, the
 * evaluator sees no country at all, and the skip list then confidently blames the rule. The
 * `unread` block is the answer to that, and it is rendered above the winner so it is read
 * first — an answer about the input belongs above the answer about the chain.
 *
 * The draft box is why this is worth opening *before* saving: an unsaved rule is evaluated
 * at position −1 through the same validator a save would use, so the screen refuses exactly
 * what the save refuses rather than accepting a rule the editor will reject a round trip
 * later. The server has carried that field since the endpoint shipped; this is its first
 * caller.
 */
function Simulator({
  targets,
  draft,
}: {
  targets: string[];
  draft: RuleDraft | null;
}) {
  const [payload, setPayload] = useState("{\n  \"country\": \"TR\",\n  \"email\": \"visitor@example.invalid\"\n}");
  const [answer, setAnswer] = useState<SimulateAnswer | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [running, setRunning] = useState(false);

  const run = async () => {
    setRunning(true);
    try {
      const parsed: unknown = JSON.parse(payload);
      setAnswer(
        await request<SimulateAnswer>("/api/v1/crm/assignment/simulate", {
          method: "POST",
          body: JSON.stringify({
            payload: parsed,
            // Only sent while the editor is open. The rules are the same rows either way;
            // the draft is prepended at position −1 so "let me try this one first" means
            // what it says to the person typing it.
            ...(draft ? { draft: draftToServer(draft) } : {}),
          }),
        }),
      );
      setError(null);
    } catch (caught) {
      setError(
        caught instanceof SyntaxError
          ? `That is not JSON: ${caught.message}`
          : caught instanceof ApiError
            ? caught.message
            : String(caught),
      );
    } finally {
      setRunning(false);
    }
  };

  return (
    <section
      aria-labelledby="simulator-heading"
      data-testid="assignment-simulator"
      className="space-y-3 rounded-md border border-line p-4"
    >
      <div>
        <h3 id="simulator-heading" className="text-[13.5px] font-medium text-ink">
          Simulator
        </h3>
        <p className="mt-0.5 text-[11.5px] text-muted">
          Paste a submission and see which rule would win. This writes nothing and does not
          advance the round-robin cursor, so running it never changes who gets the next lead.
        </p>
        {draft ? (
          <p className="mt-1 text-[11.5px] text-muted" data-testid="simulator-draft-note">
            {draft.name ? (
              <>
                Evaluating the unsaved rule <span className="text-ink">{draft.name}</span>{" "}
                above every saved rule.
              </>
            ) : (
              "Evaluating the unsaved rule above every saved rule — give it a name to see it named back."
            )}
          </p>
        ) : null}
      </div>

      <textarea
        value={payload}
        onChange={(event) => setPayload(event.target.value)}
        rows={5}
        spellCheck={false}
        aria-label="Simulator payload"
        data-testid="simulator-payload"
        className="w-full rounded-md border border-line bg-paper px-2.5 py-2 font-mono text-[12px] text-ink"
      />

      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={() => void run()}
          disabled={running}
          data-testid="simulator-run"
          className="rounded-md border border-ink px-3 py-1.5 text-[12.5px] text-ink disabled:opacity-50"
        >
          {running ? "Working…" : "Run the simulator"}
        </button>
        {targets.length ? (
          <span className="text-[11px] text-muted">
            targets: {targets.map((t) => TARGET_LABEL[t] ?? t).join(", ")}
          </span>
        ) : null}
      </div>

      {error ? (
        <p role="alert" data-testid="simulator-error" className="text-[12px] text-danger">
          {error}
        </p>
      ) : null}

      {answer ? <SimulatorResult answer={answer} /> : null}
    </section>
  );
}

function SimulatorResult({ answer }: { answer: SimulateAnswer }) {
  const { outcome } = answer;
  return (
    <div className="space-y-2" data-testid="simulator-result">
      {/* The input verdict comes first, because it can invalidate the rest. An operator who
          pastes `contury` and reads "your country rule did not match" edits the rule; told
          first that the paste was never read, they fix the paste. */}
      <UnreadKeys unread={answer.unread} readable={answer.readable_keys} />

      <p
        data-testid="simulator-winner"
        className="rounded-md border border-line bg-line/20 px-3 py-2 text-[12.5px] text-ink"
      >
        {outcome.rule_name ? (
          <>
            <span className="font-medium">{outcome.rule_name}</span> wins →{" "}
            {outcome.target_kind === "queue"
              ? "the unassigned queue"
              : outcome.target_kind === "pool"
                ? `round-robin slot ${outcome.cursor_before ?? 0} → ${short(outcome.owner_user_id)}`
                : short(outcome.owner_user_id)}
          </>
        ) : (
          <>Nothing matched — the lead lands in the unassigned queue.</>
        )}
      </p>

      {outcome.skipped.length ? (
        <ul className="space-y-1" data-testid="simulator-skipped">
          {outcome.skipped.map((rule) => (
            <li
              key={rule.rule_id}
              data-testid="simulator-skip"
              data-failed-on={rule.failed_on}
              className="text-[11.5px] text-muted"
            >
              passed over <span className="text-ink">{rule.rule_name}</span> —{" "}
              <code className="text-[11px]">{rule.failed_on}</code> did not match
            </li>
          ))}
        </ul>
      ) : null}

      <InputRead read={answer.input_read} />

      <p className="text-[11px] text-muted">
        {answer.rules_considered} rule{answer.rules_considered === 1 ? "" : "s"} considered ·{" "}
        {answer.wrote_nothing ? "nothing was written" : "something was written"}
      </p>
    </div>
  );
}

/**
 * The keys the paste carried that no rule can see.
 *
 * This is the only place a simulator can catch "you spelled it wrong", because the rule list
 * answers a different question: it explains why a *rule* lost, and it will happily blame the
 * rule for a payload the reader never understood. Rendered as a warning rather than as a
 * refusal — an unread key is frequently harmless (`utm_source` is genuinely not a condition),
 * and a screen that blocks on it would be teaching operators to ignore the block.
 *
 * The readable vocabulary is rendered from the server's own answer rather than from a list in
 * this file, so the hint and the reader cannot drift apart.
 */
function UnreadKeys({ unread, readable }: { unread: UnreadKey[]; readable: ReadableKey[] }) {
  const spellings = readable.flatMap((row) => row.aliases);
  return (
    <div data-testid="simulator-unread">
      {unread.length === 0 ? (
        <p className="text-[11.5px] text-muted">
          Every key in this payload is one a rule can read.
        </p>
      ) : (
        <>
          <p
            role="alert"
            data-testid="simulator-unread-warning"
            className="rounded-md border border-danger/40 bg-danger/5 px-3 py-2 text-[12px] text-danger"
          >
            {unread.length} key{unread.length === 1 ? "" : "s"} in this payload{" "}
            {unread.length === 1 ? "is" : "are"} not read by any rule
            {unread.some((u) => u.did_you_mean) ? " — check the spelling below" : ""}.
          </p>
          <ul className="space-y-1" data-testid="simulator-unread-list">
            {unread.map((key) => (
              <li key={key.key} data-testid="simulator-unread-key" className="text-[11.5px]">
                <code className="text-ink">{key.key}</code>{" "}
                {key.did_you_mean ? (
                  <>
                    is not a key any rule reads — did you mean{" "}
                    <code className="text-ink">{key.did_you_mean}</code>?
                  </>
                ) : (
                  <span className="text-muted">
                    is not a key any rule reads
                    {spellings.length ? (
                      <>
                        {" "}
                        (the reader looks for {spellings.slice(0, 6).join(", ")}
                        {spellings.length > 6 ? ", …" : ""})
                      </>
                    ) : null}
                  </span>
                )}
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}

/**
 * What the evaluator actually read, one line per condition key.
 *
 * Without it the only view of the input is what the operator typed, and the two differ
 * whenever a key was ignored — which is exactly the case the unread block is about, so
 * printing "country: not found" beside the warning is what turns a guess into a fact.
 */
function InputRead({ read }: { read: Record<string, string | boolean | null> }) {
  const rows = Object.entries(read);
  if (!rows.length) return null;
  return (
    <div className="text-[11px]" data-testid="simulator-input-read">
      <p className="text-muted">What the evaluator read:</p>
      <ul className="mt-0.5 flex flex-wrap gap-x-3 gap-y-0.5 text-muted">
        {rows.map(([key, value]) => (
          <li key={key} data-testid="simulator-input-read-item" data-condition={key}>
            <code>{key}</code>:{" "}
            <span className="text-ink" data-absent={value === null || value === undefined}>
              {value === null || value === undefined
                ? "not found"
                : value === false
                  ? "no"
                  : value === true
                    ? "yes"
                    : String(value)}
            </span>
          </li>
        ))}
      </ul>
    </div>
  );
}

// -------------------------------------------------------------------------------------------
// The editor
// -------------------------------------------------------------------------------------------

interface RuleDraft {
  id: string | null;
  name: string;
  /** Only the keys the operator ticked: an absent key is "no opinion", which is not the
   *  same as a present key with an empty list. */
  conditions: Partial<Record<string, string[]>>;
  target_kind: string;
  target_user_id: string;
  pool_user_ids: string[];
  active: boolean;
}

/**
 * A rule, as a form.
 *
 * `conditions` is a map of key → list of values, because the underlying document is
 * key → list and the difference between "no opinion" (key absent) and "matches nothing"
 * (key present, empty list) is the one an operator most often gets wrong. The UI therefore
 * never produces an empty list for a key the operator touched: a checked key with a blank
 * value is refused, in the editor, with the key named — the same refusal the server makes,
 * arriving before the round trip.
 */
function RuleEditor({
  draft,
  targets,
  onPreview,
  onCancel,
  onSave,
}: {
  draft: AssignmentRule | null;
  targets: string[];
  /** Publish the form as it stands so the simulator can evaluate it without saving it. */
  onPreview: (draft: RuleDraft) => void;
  onCancel: () => void;
  onSave: (draft: RuleDraft) => void;
}) {
  const initial = useMemo<RuleDraft>(() => {
    if (!draft) {
      return {
        id: null,
        name: "",
        conditions: {},
        target_kind: targets[1] ?? targets[0] ?? "queue",
        target_user_id: "",
        pool_user_ids: [],
        active: true,
      };
    }
    const conditions: Partial<Record<string, string[]>> = {};
    for (const { key } of CONDITION_KEYS) {
      const value = draft.conditions[key];
      if (Array.isArray(value)) conditions[key] = value.map(String);
    }
    return {
      id: draft.id,
      name: draft.name,
      conditions,
      target_kind: draft.target_kind,
      target_user_id: draft.target_user_id ?? "",
      pool_user_ids: [...draft.pool_user_ids],
      active: draft.active,
    };
  }, [draft, targets]);

  const [form, setForm] = useState<RuleDraft>(initial);
  const [poolText, setPoolText] = useState(initial.pool_user_ids.join("\n"));

  /** The keys the operator asked for but left blank, named for the refusal below. */
  const emptyKeys = CONDITION_KEYS.filter(({ key }) => {
    const values = form.conditions[key];
    return values !== undefined && values.length === 0;
  }).map(({ key }) => key);

  const canSave =
    form.name.trim().length > 0 &&
    emptyKeys.length === 0 &&
    (form.target_kind !== "user" || form.target_user_id.trim().length > 0) &&
    (form.target_kind !== "pool" || form.pool_user_ids.length > 0);

  // The pool textarea is free text, so the parsed members have to be folded back into the
  // form before the preview can be right — otherwise the simulator would evaluate a pool of
  // zero against a textarea listing three people, and report a rule as dead for the reason
  // that the control above it is a textarea. Publishing on every keystroke is free here: the
  // simulator only reads it when someone presses Run.
  const poolIds = useMemo(
    () =>
      poolText
        .split(/[\s,]+/)
        .map((v) => v.trim())
        .filter(Boolean),
    [poolText],
  );

  const live: RuleDraft = useMemo(
    () => ({
      ...form,
      pool_user_ids: form.target_kind === "pool" ? poolIds : [],
      // An empty name would be refused by the save, and the simulator must refuse it too —
      // a preview that names a rule the editor cannot save is the licence this REQ forbids.
      name: form.name.trim(),
    }),
    [form, poolIds],
  );

  useEffect(() => {
    onPreview(live);
  }, [live, onPreview]);

  const submit = () => {
    // A ticked-but-blank key is dropped rather than sent as an empty list: the editor
    // refuses to save while one exists, and dropping it here means the document that does
    // reach the server can only contain keys with values in them.
    const conditions: Record<string, string[]> = {};
    for (const { key } of CONDITION_KEYS) {
      const values = form.conditions[key];
      if (values && values.length > 0) conditions[key] = values;
    }
    onSave({
      id: form.id,
      name: form.name.trim(),
      conditions,
      target_kind: form.target_kind,
      target_user_id: form.target_kind === "user" ? form.target_user_id.trim() : "",
      pool_user_ids:
        form.target_kind === "pool"
          ? poolText
              .split(/[\s,]+/)
              .map((v) => v.trim())
              .filter(Boolean)
          : [],
      active: form.active,
    });
  };

  return (
    <section
      aria-label={form.id ? `Edit ${form.name}` : "Add a rule"}
      data-testid="rule-editor"
      className="space-y-3 rounded-md border border-line bg-line/10 p-4"
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <Field label="Name" htmlFor="rule-name">
          <input
            id="rule-name"
            value={form.name}
            onChange={(event) => setForm({ ...form, name: event.target.value })}
            data-testid="rule-name"
            className="w-full rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
          />
        </Field>
        <Field label="Target" htmlFor="rule-target">
          <select
            id="rule-target"
            value={form.target_kind}
            onChange={(event) => setForm({ ...form, target_kind: event.target.value })}
            data-testid="rule-target"
            className="w-full rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
          >
            {targets.map((target) => (
              <option key={target} value={target}>
                {TARGET_LABEL[target] ?? target}
              </option>
            ))}
          </select>
        </Field>
      </div>

      <fieldset className="space-y-2">
        <legend className="text-[12.5px] text-ink">Conditions</legend>
        <p className="text-[11.5px] text-muted">
          Leave every box empty and the rule matches every lead. Tick one and it has to name
          at least one value — a ticked box with no value matches nothing, which is nearly
          always a mistake.
        </p>
        <div className="grid gap-2 sm:grid-cols-2">
          {CONDITION_KEYS.map(({ key, label, hint }) => {
            const enabled = form.conditions[key] !== undefined;
            return (
              <div key={key} className="flex items-center gap-2">
                <label className="flex items-center gap-1.5 text-[12px] text-ink">
                  <input
                    type="checkbox"
                    checked={enabled}
                    onChange={(event) =>
                      setForm({
                        ...form,
                        conditions: event.target.checked
                          ? { ...form.conditions, [key]: [""] }
                          : Object.fromEntries(
                              Object.entries(form.conditions).filter(([k]) => k !== key),
                            ),
                      })
                    }
                    data-testid={`rule-condition-${key}`}
                    className="h-3.5 w-3.5"
                  />
                  {label}
                </label>
                <input
                  value={form.conditions[key]?.join(", ") ?? ""}
                  disabled={!enabled}
                  onChange={(event) =>
                    setForm({
                      ...form,
                      conditions: {
                        ...form.conditions,
                        [key]: event.target.value
                          .split(",")
                          .map((v) => v.trim())
                          .filter(Boolean),
                      },
                    })
                  }
                  placeholder={hint}
                  aria-label={`${label} values`}
                  data-testid={`rule-condition-input-${key}`}
                  className="w-full rounded-md border border-line bg-paper px-2 py-1 text-[12px] disabled:opacity-40"
                />
              </div>
            );
          })}
        </div>
        {emptyKeys.length ? (
          <p
            role="alert"
            data-testid="rule-empty-condition"
            className="text-[11.5px] text-danger"
          >
            {emptyKeys.join(", ")} {emptyKeys.length === 1 ? "is" : "are"} selected but empty —
            a rule like that matches nothing.
          </p>
        ) : null}
      </fieldset>

      {form.target_kind === "user" ? (
        <Field label="Person (user id)" htmlFor="rule-user" hint="The rule hands every matching lead to this person.">
          <input
            id="rule-user"
            value={form.target_user_id}
            onChange={(event) => setForm({ ...form, target_user_id: event.target.value })}
            data-testid="rule-user"
            className="w-full rounded-md border border-line bg-paper px-2.5 py-1.5 text-[12.5px]"
          />
        </Field>
      ) : null}

      {form.target_kind === "pool" ? (
        <Field
          label="Pool members"
          htmlFor="rule-pool"
          hint="One user id per line, in the order they should be walked. The cursor advances under a lock, so two leads at once still get two different people."
        >
          <textarea
            id="rule-pool"
            rows={3}
            value={poolText}
            onChange={(event) => setPoolText(event.target.value)}
            data-testid="rule-pool"
            className="w-full rounded-md border border-line bg-paper px-2.5 py-1.5 font-mono text-[12px]"
          />
        </Field>
      ) : null}

      <div className="flex items-center gap-2">
        <button
          type="button"
          onClick={submit}
          disabled={!canSave}
          data-testid="rule-save"
          className="rounded-md bg-ink px-3 py-1.5 text-[12.5px] text-paper disabled:opacity-40"
        >
          {form.id ? "Save rule" : "Add rule"}
        </button>
        <button
          type="button"
          onClick={onCancel}
          data-testid="rule-cancel"
          className="rounded-md border border-line px-3 py-1.5 text-[12.5px] text-ink"
        >
          Cancel
        </button>
        {!canSave ? (
          <span data-testid="rule-save-why" className="text-[11.5px] text-muted">
            {emptyKeys.length
              ? "fill in or untick the empty conditions"
              : !form.name.trim()
                ? "a rule needs a name"
                : form.target_kind === "user" && !form.target_user_id.trim()
                  ? "a person-targeting rule needs a person"
                  : "a pool needs at least one member"}
          </span>
        ) : null}
      </div>
    </section>
  );
}

function Field({
  label,
  htmlFor,
  hint,
  children,
}: {
  label: string;
  htmlFor: string;
  hint?: string;
  children: React.ReactNode;
}) {
  return (
    <div>
      <label htmlFor={htmlFor} className="block text-[12.5px] text-ink">
        {label}
      </label>
      {hint ? <p className="mt-0.5 text-[11px] text-muted">{hint}</p> : null}
      <div className="mt-1">{children}</div>
    </div>
  );
}

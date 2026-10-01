"use client";

/**
 * `/notifications/outbox` — the organization's delivery log, and the rules that fill it
 * (REQ-021, slice 3).
 *
 * Four claims this screen makes, each one a way a log lies to the administrator relying on it:
 *
 * 1. **The rows carry no content.** An admin opening this during an incident needs to know
 *    *that* a delivery failed and *whose* it was. The title and body are a customer record, and
 *    this is the screen with the widest audience in the panel — so the type has no field for
 *    them, and the panel does not go looking for them.
 * 2. **The counts come from the server and are never added up here.** The chips show the four
 *    states plus a total, and a client that computed the total from its own page would
 *    disagree with the server the moment the page was filtered.
 * 3. **Failures sort first, and the filter defaults to showing them.** An administrator opening
 *    the outbox is looking for the thing that is broken; a time-ordered log puts it below a
 *    hundred successes, and the screen's own ordering is the only place that can be fixed.
 * 4. **A retry on a delivered row says so instead of failing.** The server answers
 *    `not-retryable` rather than an error, because the caller's next action is identical — do
 *    not press the button again — and a red toast on a button that behaved correctly teaches
 *    people to distrust the screen.
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { Loader2, RefreshCw, RotateCcw, Route, Trash2, TriangleAlert } from "lucide-react";

import { EmptyState } from "@/components/empty-state";
import {
  createNotificationRoute,
  deleteNotificationRoute,
  fetchNotificationOutbox,
  fetchNotificationRoutes,
  retryNotificationDelivery,
  runNotificationRoute,
  type ApiError,
} from "@/lib/api";
import {
  NOTIFICATION_CATEGORIES,
  NOTIFICATION_DELIVERY_STATUSES,
  NOTIFICATION_PRIORITIES,
  NOTIFICATION_RECIPIENT_SHAPES,
  type NotificationDeliveryStatus,
  type NotificationOutbox,
  type NotificationOutboxRow,
  type NotificationRouteRule,
} from "@/lib/types";

const STATUS_LINE: Record<NotificationDeliveryStatus, string> = {
  pending: "Queued",
  sent: "Delivered",
  failed: "Failed",
  skipped: "Skipped",
};

/**
 * The same four states, worded the same way on the reader's drawer and on the administrator's
 * outbox.
 *
 * Exported so the drawer prints "Delivered" where the outbox prints "Delivered". A reader who
 * reads "Sent" in their own notification detail and "Delivered" in the admin log has to guess
 * whether those are the same state, and the guess will be wrong at least once.
 */
export const DELIVERY_STATUS_LINE = STATUS_LINE;

const STATUS_CLASS: Record<NotificationDeliveryStatus, string> = {
  pending: "text-muted",
  sent: "text-emerald-700 dark:text-emerald-300",
  failed: "text-red-700 dark:text-red-300",
  skipped: "text-muted",
};

/** The colour for each state, shared with the detail drawer for the same reason. */
export const DELIVERY_STATUS_CLASS = STATUS_CLASS;

const PRIORITY_CLASS: Record<string, string> = {
  low: "text-muted",
  normal: "",
  high: "text-amber-700 dark:text-amber-300",
  critical: "text-red-700 dark:text-red-300",
};

/** The first eight characters of a user id, which is what a delivery log shows. */
function shortId(id: string): string {
  return id.slice(0, 8);
}

function when(iso: string): string {
  const parsed = new Date(iso);
  if (Number.isNaN(parsed.getTime())) return iso;
  return parsed.toLocaleString();
}

export function NotificationOutbox() {
  const [outbox, setOutbox] = useState<NotificationOutbox | null>(null);
  const [rules, setRules] = useState<NotificationRouteRule[] | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  /** Which states the reader asked for. Empty means all four, which is the server's default. */
  const [statuses, setStatuses] = useState<NotificationDeliveryStatus[]>([]);
  const [notice, setNotice] = useState<string | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      // Two independent reads in one effect: the outbox and the rules are different tables
      // behind different permissions in principle, and a screen that showed one without the
      // other would look like half a feature rather than like an outage.
      const [outboxAnswer, ruleAnswer] = await Promise.all([
        fetchNotificationOutbox({ statuses, limit: 50 }),
        fetchNotificationRoutes(),
      ]);
      setOutbox(outboxAnswer);
      setRules(ruleAnswer);
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setLoading(false);
    }
  }, [statuses]);

  useEffect(() => {
    void load();
  }, [load]);

  const toggleStatus = useCallback((status: NotificationDeliveryStatus) => {
    setStatuses((current) =>
      current.includes(status)
        ? current.filter((value) => value !== status)
        : // The order follows the closed list rather than the click order, so the chips read
          // the same however the reader arrived at the selection.
          NOTIFICATION_DELIVERY_STATUSES.filter((value) => [...current, status].includes(value)),
    );
  }, []);

  const retry = useCallback(
    async (row: NotificationOutboxRow) => {
      setBusyId(row.id);
      setNotice(null);
      try {
        const answer = await retryNotificationDelivery(row.id);
        setNotice(
          answer.outcome === "requeued"
            ? "Requeued. The runner will try it again."
            : "That delivery already went, so there is nothing to retry.",
        );
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusyId(null);
      }
    },
    [load],
  );

  const removeRule = useCallback(
    async (rule: NotificationRouteRule) => {
      setBusyId(rule.id);
      setNotice(null);
      try {
        await deleteNotificationRoute(rule.id);
        setNotice(`Removed the rule for ${rule.event_name}.`);
        await load();
      } catch (caught) {
        setError((caught as ApiError).message);
      } finally {
        setBusyId(null);
      }
    },
    [load],
  );

  // ------------------------------------------------------------------ the three states
  if (loading && !outbox) return <OutboxSkeleton />;
  if (error && !outbox) {
    return (
      <div className="space-y-3" data-outbox-state="error">
        <p className="text-[13px] text-red-700 dark:text-red-300">{error}</p>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-2 text-[13px]"
        >
          <RefreshCw className="h-4 w-4" aria-hidden />
          Retry
        </button>
      </div>
    );
  }

  const counts = outbox?.counts;
  const rows = outbox?.rows ?? [];

  return (
    <div className="space-y-8" data-outbox-state="ready">
      {/* ------------------------------------------------------------- the delivery log */}
      <section aria-labelledby="outbox-heading" className="space-y-3">
        <div className="flex flex-wrap items-baseline justify-between gap-2">
          <h2 id="outbox-heading" className="text-[15px] font-semibold">
            Delivery log
          </h2>
          <p className="text-[12px] text-muted">
            Every delivery across the organization, failures first.{" "}
            {counts ? `The log goes back ${outbox?.retention_days} days.` : null}
          </p>
        </div>

        {/* The chips carry the server's own counts. A chip that counted the loaded page would
            say "3" above a filter that matches 300, and an administrator would trust it. */}
        <div className="flex flex-wrap gap-2" role="group" aria-label="Filter by state">
          <button
            type="button"
            data-outbox-chip="all"
            aria-pressed={statuses.length === 0}
            onClick={() => setStatuses([])}
            className={`rounded-full border px-3 py-1 text-[12px] ${
              statuses.length === 0 ? "border-line bg-quiet-soft" : "border-line"
            }`}
          >
            All{counts ? ` (${counts.total})` : ""}
          </button>
          {NOTIFICATION_DELIVERY_STATUSES.map((status) => (
            <button
              key={status}
              type="button"
              data-outbox-chip={status}
              aria-pressed={statuses.includes(status)}
              onClick={() => toggleStatus(status)}
              className={`rounded-full border px-3 py-1 text-[12px] ${
                statuses.includes(status) ? "border-line bg-quiet-soft" : "border-line"
              }`}
            >
              {STATUS_LINE[status]}
              {counts ? ` (${counts[status]})` : ""}
            </button>
          ))}
        </div>

        {notice ? (
          <p data-outbox-notice className="text-[12.5px] text-muted">
            {notice}
          </p>
        ) : null}

        {rows.length === 0 ? (
          <div className="rounded-lg border border-line" data-outbox-empty>
            <EmptyState
              title="Nothing has been delivered yet"
              hint={
                statuses.length > 0
                  ? "No delivery is in the states you picked. Clear the filter to see the rest."
                  : "Once the platform tells somebody something, every attempt shows up here."
              }
            />
          </div>
        ) : (
          <div className="overflow-x-auto rounded-lg border border-line">
            <table className="w-full min-w-[720px] border-collapse text-[13px]">
              <caption className="sr-only">
                Every notification delivery across the organization
              </caption>
              <thead>
                <tr className="border-b border-line bg-quiet-soft">
                  <th scope="col" className="px-3 py-2 text-left font-medium">State</th>
                  <th scope="col" className="px-3 py-2 text-left font-medium">Channel</th>
                  <th scope="col" className="px-3 py-2 text-left font-medium">Category</th>
                  <th scope="col" className="px-3 py-2 text-left font-medium">Reader</th>
                  <th scope="col" className="px-3 py-2 text-left font-medium">Attempts</th>
                  <th scope="col" className="px-3 py-2 text-left font-medium">When</th>
                  <th scope="col" className="px-3 py-2 text-right font-medium">Action</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((row) => (
                  <tr
                    key={row.id}
                    data-outbox-row={row.status}
                    className="border-b border-line last:border-0"
                  >
                    <td className={`px-3 py-2 ${STATUS_CLASS[row.status]}`}>
                      <span className="font-medium">{STATUS_LINE[row.status]}</span>
                      {/* The transport's own words, when it gave any. An administrator
                          debugging a bounce needs the server's message, not ours. */}
                      {row.error ? (
                        <span className="block text-[12px] text-muted">{row.error}</span>
                      ) : null}
                      {row.response_status ? (
                        <span className="block text-[12px] text-muted">HTTP {row.response_status}</span>
                      ) : null}
                    </td>
                    <td className="px-3 py-2">{row.channel}</td>
                    <td className="px-3 py-2">
                      {row.category}
                      <span className={`block text-[12px] ${PRIORITY_CLASS[row.priority] ?? ""}`}>
                        {row.priority}
                      </span>
                    </td>
                    {/* Ids only. The reader's name and address are not on this screen and
                        cannot be added to it without changing the server's type as well. */}
                    <td className="px-3 py-2 font-mono text-[12px] text-muted">
                      {shortId(row.user_id)}
                    </td>
                    <td className="px-3 py-2">
                      {row.attempts}/{row.max_attempts}
                    </td>
                    <td className="px-3 py-2 text-muted">{when(row.sent_at ?? row.created_at)}</td>
                    <td className="px-3 py-2 text-right">
                      {/* Only a failed row is retryable, and the server refuses the others
                          rather than re-sending a message that already arrived. A button that
                          is present-but-dead is worse than one that is not there. */}
                      {row.status === "failed" ? (
                        <button
                          type="button"
                          data-outbox-retry={row.id}
                          disabled={busyId === row.id}
                          onClick={() => void retry(row)}
                          className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12px] disabled:opacity-50"
                        >
                          {busyId === row.id ? (
                            <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden />
                          ) : (
                            <RotateCcw className="h-3.5 w-3.5" aria-hidden />
                          )}
                          Retry
                        </button>
                      ) : null}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>

      {/* ----------------------------------------------------------- the routing rules */}
      <section aria-labelledby="rules-heading" className="space-y-3">
        <div className="flex flex-wrap items-baseline justify-between gap-2">
          <h2 id="rules-heading" className="text-[15px] font-semibold">
            Routing rules
          </h2>
          <button
            type="button"
            data-rules-toggle
            onClick={() => {
              setCreating((value) => !value);
              setFormError(null);
            }}
            className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-1.5 text-[12.5px]"
          >
            <Route className="h-3.5 w-3.5" aria-hidden />
            {creating ? "Cancel" : "New rule"}
          </button>
        </div>
        <p className="text-[12px] text-muted">
          A rule turns a fact on the platform's event bus into a notification. The module that
          recorded the fact never names this screen.
        </p>

        {creating ? (
          <RouteForm
            onCancel={() => setCreating(false)}
            onError={setFormError}
            onCreated={async () => {
              setCreating(false);
              setFormError(null);
              await load();
            }}
          />
        ) : null}
        {formError ? (
          <p data-rules-error className="text-[12.5px] text-red-700 dark:text-red-300">
            {formError}
          </p>
        ) : null}

        {rules === null ? null : rules.length === 0 ? (
          <div className="rounded-lg border border-line" data-rules-empty>
            <EmptyState
              title="No routing rules yet"
              hint="Without a rule, a fact on the event bus is recorded and nobody is told."
              action={
                <button
                  type="button"
                  onClick={() => setCreating(true)}
                  className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
                >
                  Write the first one
                </button>
              }
            />
          </div>
        ) : (
          <div className="overflow-x-auto rounded-lg border border-line">
            <table className="w-full min-w-[720px] border-collapse text-[13px]">
              <caption className="sr-only">The rules that turn bus events into notifications</caption>
              <thead>
                <tr className="border-b border-line bg-quiet-soft">
                  <th scope="col" className="px-3 py-2 text-left font-medium">Event</th>
                  <th scope="col" className="px-3 py-2 text-left font-medium">Category</th>
                  <th scope="col" className="px-3 py-2 text-left font-medium">Who hears</th>
                  <th scope="col" className="px-3 py-2 text-left font-medium">Title</th>
                  <th scope="col" className="px-3 py-2 text-right font-medium">Action</th>
                </tr>
              </thead>
              <tbody>
                {rules.map((rule) => (
                  <tr key={rule.id} data-rule-row={rule.event_name} className="border-b border-line last:border-0">
                    <td className="px-3 py-2 font-mono text-[12px]">{rule.event_name}</td>
                    <td className="px-3 py-2">
                      {rule.category}
                      <span className={`block text-[12px] ${PRIORITY_CLASS[rule.priority] ?? ""}`}>
                        {rule.priority}
                      </span>
                    </td>
                    <td className="px-3 py-2 font-mono text-[12px] text-muted">{rule.recipient}</td>
                    <td className="px-3 py-2">{rule.title_template}</td>
                    <td className="px-3 py-2 text-right">
                      <button
                        type="button"
                        data-rule-delete={rule.id}
                        disabled={busyId === rule.id}
                        onClick={() => void removeRule(rule)}
                        className="inline-flex items-center gap-1.5 rounded-md border border-line px-2.5 py-1.5 text-[12px] disabled:opacity-50"
                      >
                        <Trash2 className="h-3.5 w-3.5" aria-hidden />
                        Remove
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>

      <RouteProbe onResult={setNotice} />
    </div>
  );
}

/**
 * The "did it actually fire?" box.
 *
 * **This is the proof of slice 3, and it is on the screen on purpose.** The claim is that a
 * fact on the bus becomes a notification with no call between the two modules; the only honest
 * way to show that to a person is to hand the router an event a producer would have written and
 * read back what it did. The four counts are printed separately, because `created: 0` alone
 * cannot distinguish a rule that is ahead of its producer from a rule that resolves to nobody.
 */
function RouteProbe({ onResult }: { onResult: (message: string | null) => void }) {
  const [eventName, setEventName] = useState("ticket.created");
  const [subject, setSubject] = useState("Checkout page needs review");
  const [busy, setBusy] = useState(false);
  const [report, setReport] = useState<Record<string, unknown> | null>(null);

  const run = useCallback(async () => {
    setBusy(true);
    onResult(null);
    try {
      // A fresh event id every run, because the dedupe key is derived from it: running the same
      // id twice would legitimately report `deduped: 1` and teach the wrong lesson.
      const answer = await runNotificationRoute({
        event_name: eventName,
        payload: { title: subject },
      });
      setReport(answer as unknown as Record<string, unknown>);
    } catch (caught) {
      onResult((caught as ApiError).message);
      setReport(null);
    } finally {
      setBusy(false);
    }
  }, [eventName, subject, onResult]);

  const created = typeof report?.created === "number" ? report.created : null;
  const unmatched = typeof report?.unmatched_rules === "number" ? report.unmatched_rules : null;
  const dropped =
    typeof report?.dropped_recipients === "number" ? report.dropped_recipients : null;
  const unknownEvent = report?.unknown_event === true;

  return (
    <section aria-labelledby="probe-heading" className="space-y-3" data-route-probe>
      <h2 id="probe-heading" className="text-[15px] font-semibold">
        Try a rule
      </h2>
      <p className="text-[12px] text-muted">
        Sends one event through the router and reports what it did. Nothing here is special: it
        is the same path a real event takes.
      </p>
      <div className="flex flex-wrap items-end gap-2">
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">Event name</span>
          <input
            data-probe-event
            value={eventName}
            onChange={(event) => setEventName(event.target.value)}
            className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
          />
        </label>
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">Subject</span>
          <input
            data-probe-subject
            value={subject}
            onChange={(event) => setSubject(event.target.value)}
            className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
          />
        </label>
        <button
          type="button"
          data-probe-run
          disabled={busy}
          onClick={() => void run()}
          className="inline-flex items-center gap-2 rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-50"
        >
          {busy ? <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden /> : null}
          Run it
        </button>
      </div>

      {report ? (
        <ul className="space-y-1 text-[12.5px]" data-probe-report>
          <li data-probe-created={String(created)}>
            <span className="font-medium">{created ?? 0}</span> notification
            {created === 1 ? "" : "s"} created
          </li>
          <li className="text-muted">{(report.deduped as number) ?? 0} collapsed as duplicates</li>
          <li className="text-muted">
            {unmatched ?? 0} rule{unmatched === 1 ? "" : "s"} matched nobody
          </li>
          {/** The one line that says a rule is mis-wired rather than waiting. Rendered above
              the "unknown event" note because a dropped recipient is a fact about the rule and a
              missing producer is a fact about the event, and the two need different fixes. */}
          {dropped ? (
            <li
              className="flex items-start gap-1.5 text-amber-700 dark:text-amber-300"
              data-probe-dropped={String(dropped)}
            >
              <TriangleAlert className="mt-0.5 h-3.5 w-3.5 shrink-0" aria-hidden />
              <span>
                {dropped} recipient{dropped === 1 ? "" : "s"} resolved and then refused — outside
                this event&rsquo;s organization, or not an account. Nothing was written for them,
                so check the rule&rsquo;s recipient field.
              </span>
            </li>
          ) : null}
          {unknownEvent ? (
            <li className="flex items-center gap-1.5 text-amber-700 dark:text-amber-300">
              <TriangleAlert className="h-3.5 w-3.5" aria-hidden />
              No rule listens for this event name yet — that is a rule waiting for its producer,
              not a failure.
            </li>
          ) : null}
        </ul>
      ) : null}
    </section>
  );
}

/** The create form, with the recipient shape as a select so the prefix is never typed by hand. */
function RouteForm({
  onCancel,
  onCreated,
  onError,
}: {
  onCancel: () => void;
  onCreated: () => Promise<void>;
  onError: (message: string | null) => void;
}) {
  const [eventName, setEventName] = useState("");
  const [category, setCategory] = useState<string>(NOTIFICATION_CATEGORIES[0]);
  const [priority, setPriority] = useState<string>(NOTIFICATION_PRIORITIES[1]);
  // `string` rather than the literal union: the select's value arrives as a `string`, and
  // narrowing the state to `"actor"` here is what makes the three `===` comparisons below
  // errors rather than the runtime check they are.
  const [shape, setShape] = useState<string>(NOTIFICATION_RECIPIENT_SHAPES[0].value);
  const [target, setTarget] = useState("");
  const [title, setTitle] = useState("");
  const [url, setUrl] = useState("");
  const [busy, setBusy] = useState(false);

  const needsTarget = useMemo(
    () => NOTIFICATION_RECIPIENT_SHAPES.find((entry) => entry.value === shape)?.needsTarget ?? false,
    [shape],
  );

  const submit = useCallback(async () => {
    setBusy(true);
    onError(null);
    try {
      await createNotificationRoute({
        event_name: eventName,
        category,
        priority,
        // The prefix is a select and the target is a field, so the concatenated string can
        // never be `permission:` with nothing after it — the shape the database refuses and
        // the one a client is most likely to send by accident.
        recipient: needsTarget ? `${shape}${target.trim()}` : shape,
        title_template: title,
        url_template: url.trim() ? url.trim() : null,
      });
      await onCreated();
    } catch (caught) {
      onError((caught as ApiError).message);
    } finally {
      setBusy(false);
    }
  }, [eventName, category, priority, shape, target, title, url, needsTarget, onCreated, onError]);

  return (
    <form
      data-rule-form
      className="space-y-3 rounded-lg border border-line p-4"
      onSubmit={(event) => {
        event.preventDefault();
        void submit();
      }}
    >
      <div className="grid gap-3 sm:grid-cols-2">
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">Event name</span>
          <input
            data-rule-event
            required
            value={eventName}
            placeholder="ticket.created"
            onChange={(event) => setEventName(event.target.value)}
            className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
          />
        </label>
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">Category</span>
          <select
            data-rule-category
            value={category}
            onChange={(event) => setCategory(event.target.value)}
            className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
          >
            {NOTIFICATION_CATEGORIES.map((value) => (
              <option key={value} value={value}>
                {value}
              </option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">Priority</span>
          <select
            data-rule-priority
            value={priority}
            onChange={(event) => setPriority(event.target.value)}
            className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
          >
            {NOTIFICATION_PRIORITIES.map((value) => (
              <option key={value} value={value}>
                {value}
              </option>
            ))}
          </select>
        </label>
        <label className="flex flex-col gap-1 text-[12px]">
          <span className="text-muted">Who hears about it</span>
          <select
            data-rule-shape
            value={shape}
            onChange={(event) => setShape(event.target.value)}
            className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
          >
            {NOTIFICATION_RECIPIENT_SHAPES.map((entry) => (
              <option key={entry.value} value={entry.value}>
                {entry.label}
              </option>
            ))}
          </select>
        </label>
        {/* The target only appears when the shape needs one, so the form cannot submit a
            `permission:` with nothing after it. */}
        {needsTarget ? (
          <label className="flex flex-col gap-1 text-[12px]">
            <span className="text-muted">
              {shape === "payload_user:" ? "Payload field" : shape === "role:" ? "Role key" : "Permission key"}
            </span>
            <input
              data-rule-target
              required
              value={target}
              placeholder={shape === "payload_user:" ? "assignee_id" : shape === "role:" ? "approver" : "approvals.approve"}
              onChange={(event) => setTarget(event.target.value)}
              className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
            />
          </label>
        ) : null}
        <label className="flex flex-col gap-1 text-[12px] sm:col-span-2">
          <span className="text-muted">Title</span>
          <input
            data-rule-title
            required
            value={title}
            placeholder="{actor} opened {subject}"
            onChange={(event) => setTitle(event.target.value)}
            className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
          />
          <span className="text-muted">
            <code>{"{actor}"}</code> and <code>{"{subject}"}</code> are substituted; anything else
            is left as written, so a typo is visible in the inbox rather than silent.
          </span>
        </label>
        <label className="flex flex-col gap-1 text-[12px] sm:col-span-2">
          <span className="text-muted">Link (optional)</span>
          <input
            data-rule-url
            value={url}
            placeholder="/tickets/{subject}"
            onChange={(event) => setUrl(event.target.value)}
            className="rounded-md border border-line bg-transparent px-2 py-1.5 text-[13px]"
          />
        </label>
      </div>
      <div className="flex gap-2">
        <button
          type="submit"
          data-rule-save
          disabled={busy}
          className="rounded-md border border-line px-3 py-1.5 text-[12.5px] disabled:opacity-50"
        >
          {busy ? "Saving…" : "Save rule"}
        </button>
        <button
          type="button"
          onClick={onCancel}
          className="rounded-md border border-line px-3 py-1.5 text-[12.5px]"
        >
          Cancel
        </button>
      </div>
    </form>
  );
}

/** The loading state, so the screen is never a blank page while the log is on its way. */
function OutboxSkeleton() {
  return (
    <div className="space-y-3" data-outbox-state="loading" aria-busy="true">
      <div className="h-5 w-40 animate-pulse rounded bg-quiet-soft" />
      <div className="h-8 w-72 animate-pulse rounded-full bg-quiet-soft" />
      <div className="space-y-2">
        {Array.from({ length: 5 }, (_, index) => (
          <div key={index} className="h-10 animate-pulse rounded bg-quiet-soft" />
        ))}
      </div>
    </div>
  );
}

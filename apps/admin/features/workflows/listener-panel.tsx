"use client";

/**
 * *Listen for a real event* — the builder's inspector panel (REQ-004 slice 3, criterion 5).
 *
 * The panel is a self-contained component rather than a section of the 3,000-line builder
 * for one reason that shows up in the criterion: **it has its own lifetime**. It polls while
 * a listener is armed, stops the moment one is captured, and starts again on the next arm.
 * Inlining that into the builder would put a `setInterval` next to the autosave timer and
 * the conflict watcher, and the two would share a lifetime by accident.
 *
 * The three rules it holds, each in `test-listener.ts` and each with a test:
 *
 *  * **arming and reading are two calls.** The poll is a `GET`; only the button is a `POST`.
 *    A poll that arms re-arms the row the matcher is about to fill, and the capture appears
 *    for one frame — which is the same as never appearing.
 *  * **the control knows why it is off.** A non-trigger node, nothing selected, or a node
 *    already listening each produce a sentence, because a disabled control with no reason
 *    is a dead control wearing a disabled attribute.
 *  * **the countdown comes from the expiry instant**, not from a local tick counter, so a
 *    backgrounded tab does not come back claiming four minutes on a listener that expired
 *    while it was away.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { AlertTriangle, CheckCircle2, Ear, Loader2, Radio, TimerOff } from "lucide-react";

import {
  armWorkflowListener,
  readWorkflowListeners,
  type WorkflowListener,
} from "@/lib/api";
import {
  formatDuration,
  payloadSource,
  secondsLeft,
  startability,
  summarySentence,
  type ListenerRow,
} from "@/features/workflows/test-listener";

/** How often the panel re-reads while something is armed. */
const POLL_MS = 2_000;

export function ListenerPanel({
  workflowId,
  selected,
}: {
  workflowId: string;
  /** The canvas selection, already reduced to node shape by the builder. */
  selected: { id: string; type: string; label: string }[] | null;
}) {
  const [rows, setRows] = useState<ListenerRow[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [arming, setArming] = useState(false);
  const [armError, setArmError] = useState<string | null>(null);
  const [token, setToken] = useState<string | null>(null);
  // `now` is state rather than `Date.now()` in the render body, because the countdown has
  // to re-render. One timer drives it whether or not a listener is armed, so a captured
  // payload does not stop the clock that draws the next one's expiry.
  const [now, setNow] = useState(() => Date.now());
  // The armed count is read inside the effect and the effect must not re-run because it
  // changed, or every tick would tear down and re-arm the interval below.
  const armedRef = useRef(0);

  const read = useCallback(async () => {
    try {
      const body = await readWorkflowListeners(workflowId);
      setRows((body.listeners ?? []) as ListenerRow[]);
      setLoadError(null);
      setLoaded(true);
    } catch (err) {
      setLoadError(err instanceof Error ? err.message : "The listeners could not be read.");
      setLoaded(true);
    }
  }, [workflowId]);

  useEffect(() => {
    void read();
  }, [read]);

  useEffect(() => {
    const armed = rows.filter(
      (row) => row.status === "armed" && secondsLeft(row, Date.now()) > 0,
    ).length;
    armedRef.current = armed;
  }, [rows]);

  useEffect(() => {
    const timer = setInterval(() => {
      setNow(Date.now());
      // Only poll while something is waiting. Polling forever is a request a minute for
      // the lifetime of a tab that is showing a captured payload nobody is looking at.
      if (armedRef.current > 0) void read();
    }, POLL_MS);
    return () => clearInterval(timer);
  }, [read]);

  const answer = startability(selected, rows, now);
  const captured = payloadSource(rows, now);
  const summary = summarySentence(rows, now);

  const arm = useCallback(async () => {
    if (!answer.canListen || !answer.nodeId) return;
    setArming(true);
    setArmError(null);
    try {
      const body = await armWorkflowListener(workflowId, answer.nodeId);
      // The token is shown once and then only read back by the author. It is a handle, not
      // a credential, and showing it in the panel is the point of returning it at all.
      setToken(body.token);
      await read();
    } catch (err) {
      setArmError(err instanceof Error ? err.message : "The listener could not be armed.");
    } finally {
      setArming(false);
    }
  }, [answer.canListen, answer.nodeId, read, workflowId]);

  return (
    <section className="rounded-md border border-line p-2" data-builder-listener>
      <div className="flex items-center gap-1.5">
        <h3 className="text-[12px] font-medium">Test event</h3>
        {armedRef.current > 0 ? (
          <Radio className="h-3.5 w-3.5 text-emerald-600" aria-hidden="true" data-listener-live />
        ) : null}
      </div>

      <p className="mt-0.5 text-[11.5px] text-muted" data-listener-summary>
        {summary}
      </p>

      {/* The control. Disabled **with its reason**, which is the whole difference between
          a control that cannot be pressed now and a control that is broken. */}
      <button
        type="button"
        onClick={() => void arm()}
        disabled={!answer.canListen || arming}
        className="mt-2 inline-flex items-center gap-1.5 rounded-md border border-line px-2 py-1.5 text-[12.5px] disabled:cursor-not-allowed disabled:opacity-60"
        data-listener-arm
        title={answer.message ?? "Wait for the next real event this trigger fires on"}
      >
        {arming ? (
          <Loader2 className="h-3.5 w-3.5 animate-spin" aria-hidden="true" />
        ) : (
          <Ear className="h-3.5 w-3.5" aria-hidden="true" />
        )}
        Listen for a real event
      </button>
      {answer.message ? (
        <p className="mt-1 text-[11.5px] text-muted" data-listener-reason>
          {answer.message}
        </p>
      ) : null}
      {armError ? (
        <p className="mt-1 text-[11.5px] text-danger" data-listener-error>
          <AlertTriangle className="mr-1 inline h-3 w-3" aria-hidden="true" />
          {armError}
        </p>
      ) : null}
      {loadError ? (
        <p className="mt-1 text-[11.5px] text-danger" data-listener-error>
          <AlertTriangle className="mr-1 inline h-3 w-3" aria-hidden="true" />
          {loadError}
        </p>
      ) : null}

      {/* The token, once. A caller that wants to script against the armed listener needs it
          here; a caller that has scrolled past does not need it again. */}
      {token ? (
        <p className="mt-2 break-all rounded border border-line bg-quiet-soft/40 px-1.5 py-1 font-mono text-[10.5px] text-muted" data-listener-token>
          {token}
        </p>
      ) : null}

      {/* The captured payload. This is what the whole feature exists for: the shape of the
          event as it *actually* arrived, which is the only honest answer to a payload the
          documentation got wrong. */}
      {captured ? (
        <div className="mt-2" data-listener-capture={captured.id}>
          <p className="flex items-center gap-1 text-[11.5px]" data-listener-capture-status>
            <CheckCircle2 className="h-3 w-3 text-emerald-600" aria-hidden="true" />
            Captured {captured.event_name}
            {captured.status === "expired" ? (
              <span className="ml-1 inline-flex items-center gap-1 text-muted">
                <TimerOff className="h-3 w-3" aria-hidden="true" />
                {formatDuration(0)}
              </span>
            ) : null}
          </p>
          {captured.status === "armed" ? (
            <p className="mt-0.5 text-[11.5px] text-muted" data-listener-countdown>
              {formatDuration(secondsLeft(captured, now))}
            </p>
          ) : null}
          {captured.payload_text ? (
            <pre
              className="mt-1 max-h-56 overflow-auto rounded border border-line bg-quiet-soft/40 p-1.5 font-mono text-[10.5px] whitespace-pre-wrap"
              data-listener-payload
            >
              {captured.payload_text}
            </pre>
          ) : null}
        </div>
      ) : loaded ? (
        <p className="mt-2 text-[11.5px] text-muted" data-listener-empty>
          Nothing captured yet.
        </p>
      ) : (
        <p className="mt-2 text-[11.5px] text-muted" data-listener-loading>
          <Loader2 className="mr-1 inline h-3 w-3 animate-spin" aria-hidden="true" />
          Reading the listeners…
        </p>
      )}

      {/* The spent rows, because a row that quietly disappears is indistinguishable from
          one that was never armed. */}
      {rows.filter((row) => row.status === "expired").length > 0 ? (
        <p className="mt-2 text-[11px] text-muted" data-listener-expired-count>
          {rows.filter((row) => row.status === "expired").length} earlier listener
          {rows.filter((row) => row.status === "expired").length === 1 ? "" : "s"} expired
          without an event.
        </p>
      ) : null}
    </section>
  );
}

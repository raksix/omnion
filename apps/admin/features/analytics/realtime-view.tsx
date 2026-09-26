"use client";

/**
 * `/analytics/realtime` — what is happening right now (REQ-007, slice 3).
 *
 * The screen reads one snapshot over the plain endpoint and then keeps it fresh over the
 * server-sent stream: a beacon appears here within seconds, which is the acceptance criterion
 * this screen exists to satisfy. When the tab is hidden the stream is closed and the pill says
 * `Paused` — a screen nobody is looking at must not keep polling a database.
 *
 * The counters come in two windows on purpose: five minutes answers "who is here now" and thirty
 * answers "what just happened"; a single number would have to lie about which question it
 * answers.
 */

import { useCallback, useEffect, useRef, useState } from "react";

import { Radio, RefreshCw, Target } from "lucide-react";

import {
  ApiError,
  analyticsRealtimeStreamUrl,
  fetchAnalyticsRealtime,
  type AnalyticsRealtimeSnapshot,
} from "@/lib/api";

import { useAnalytics } from "./analytics-shell";
import { EmptyPanel, ErrorPanel, LastSeen, LoadingRows, Panel, formatCount } from "./parts";

/** How a live connection is doing, as the header pill reports it. */
type StreamState = "connecting" | "live" | "paused" | "closed";

/** The realtime screen. */
export function AnalyticsRealtimeView() {
  const { siteId } = useAnalytics();
  const [snapshot, setSnapshot] = useState<AnalyticsRealtimeSnapshot | null>(null);
  const [status, setStatus] = useState<"idle" | "loading" | "ready" | "error">("idle");
  const [error, setError] = useState<{ message: string; code: string } | null>(null);
  const [streamState, setStreamState] = useState<StreamState>("connecting");
  const [attempt, setAttempt] = useState(0);

  // The stream is opened, closed and re-opened by visibility; the ref survives those cycles.
  const source = useRef<EventSource | null>(null);

  const retry = useCallback(() => setAttempt((value) => value + 1), []);

  // The first snapshot is a plain request: a stream that has not opened yet must not leave the
  // screen empty, and a reader whose browser refuses `EventSource` still gets the numbers.
  useEffect(() => {
    if (!siteId) {
      setSnapshot(null);
      setStatus("idle");
      return;
    }

    let live = true;
    setStatus("loading");
    setError(null);

    fetchAnalyticsRealtime(siteId)
      .then((answer) => {
        if (live) {
          setSnapshot(answer);
          setStatus("ready");
        }
      })
      .catch((cause: unknown) => {
        if (live) {
          setStatus("error");
          setError(
            cause instanceof ApiError
              ? { message: cause.message, code: cause.code }
              : { message: "The realtime view could not be read.", code: "unknown_error" },
          );
        }
      });

    return () => {
      live = false;
    };
  }, [siteId, attempt]);

  // The stream, with the tab's own visibility as its switch.
  useEffect(() => {
    if (!siteId) {
      return;
    }
    if (typeof window === "undefined" || typeof window.EventSource === "undefined") {
      setStreamState("closed");
      return;
    }

    let closed = false;
    const close = () => {
      source.current?.close();
      source.current = null;
    };

    const open = () => {
      if (closed) {
        return;
      }
      close();
      setStreamState("connecting");
      const stream = new EventSource(analyticsRealtimeStreamUrl(siteId));
      stream.addEventListener("open", () => setStreamState("live"));
      stream.addEventListener("snapshot", (event) => {
        try {
          setSnapshot(JSON.parse((event as MessageEvent<string>).data) as AnalyticsRealtimeSnapshot);
          setStatus("ready");
        } catch {
          // A frame that cannot be parsed is dropped; the next one is five seconds away.
        }
      });
      stream.addEventListener("error", () => setStreamState("closed"));
      source.current = stream;
    };

    const onVisibility = () => {
      if (document.visibilityState === "hidden") {
        close();
        setStreamState("paused");
      } else {
        open();
        // Coming back also refreshes the counters, because the stream's first frame is a tick away.
        setAttempt((value) => value + 1);
      }
    };

    document.addEventListener("visibilitychange", onVisibility);
    if (document.visibilityState === "hidden") {
      setStreamState("paused");
    } else {
      open();
    }

    return () => {
      closed = true;
      document.removeEventListener("visibilitychange", onVisibility);
      close();
    };
  }, [siteId]);

  if (status === "error" && error) {
    return <ErrorPanel message={error.message} code={error.code} onRetry={retry} />;
  }

  if (status === "loading" && !snapshot) {
    return (
      <Panel title="Realtime" bodyClassName="p-0">
        <LoadingRows rows={4} label="Reading the last half hour" />
      </Panel>
    );
  }

  if (!snapshot) {
    return (
      <EmptyPanel title="No realtime data yet" dataAttribute="analytics-realtime-empty">
        <p>
          Realtime reads the raw rows of the last half hour — nothing to roll up first. As soon as
          a beacon lands it appears here.
        </p>
      </EmptyPanel>
    );
  }

  const pill = {
    connecting: { label: "Connecting…", className: "border-line text-muted" },
    live: { label: "Live", className: "border-accent bg-accent-soft text-accent-strong" },
    paused: { label: "Paused — tab hidden", className: "border-line text-muted" },
    closed: { label: "Stream closed — retrying", className: "border-line text-muted" },
  }[streamState];

  const counters = [
    { key: "visitors", label: "Visitors" },
    { key: "pageviews", label: "Page views" },
    { key: "events", label: "Events" },
    { key: "conversions", label: "Conversions" },
  ] as const;

  return (
    <div className="flex flex-col gap-4" data-analytics-realtime>
      <div className="flex flex-wrap items-center gap-3">
        <span
          data-rt-state={streamState}
          className={`flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-[11.5px] ${pill.className}`}
        >
          <Radio className="size-3.5" aria-hidden />
          {pill.label}
        </span>
        <span className="text-[11.5px] text-muted">
          Snapshot <LastSeen value={snapshot.generated_at} /> · a beacon shows up within seconds
        </span>
        <button
          type="button"
          data-rt-refresh
          onClick={retry}
          className="ml-auto flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-ink transition hover:bg-quiet-soft"
        >
          <RefreshCw className="size-3.5" aria-hidden />
          Refresh
        </button>
      </div>

      <div className="grid gap-4 lg:grid-cols-2">
        {([snapshot.last_5, snapshot.last_30] as const).map((window, index) => (
          <Panel
            key={window.window_minutes}
            title={`Last ${window.window_minutes} minutes`}
            testId={index === 0 ? "realtime-5" : "realtime-30"}
          >
            <dl className="grid grid-cols-2 gap-3 p-4 sm:grid-cols-4">
              {counters.map((entry) => (
                <div key={entry.key} className="flex flex-col gap-1">
                  <dt className="text-[11px] tracking-wide text-muted uppercase">{entry.label}</dt>
                  <dd
                    data-rt-counter={`${window.window_minutes}-${entry.key}`}
                    className="font-mono text-[18px] text-ink"
                  >
                    {formatCount(window[entry.key])}
                  </dd>
                </div>
              ))}
            </dl>
          </Panel>
        ))}
      </div>

      <div className="grid gap-4 lg:grid-cols-2">
        <Panel
          title="Current pages"
          subtitle="Where the traffic is, in the last thirty minutes"
          bodyClassName="p-0"
          testId="realtime-pages"
        >
          {snapshot.pages.length === 0 ? (
            <p className="px-4 py-4 text-[12.5px] text-muted">
              Nobody has read a page in the last half hour.
            </p>
          ) : (
            <ul data-rt-pages className="flex flex-col divide-y divide-line">
              {snapshot.pages.map((page) => (
                <li key={page.path} className="flex items-center gap-3 px-4 py-2.5">
                  <span className="min-w-0 flex-1 truncate font-mono text-[12px] text-ink">
                    {page.path}
                  </span>
                  <span className="font-mono text-[12px] text-ink">{formatCount(page.visitors)}</span>
                  <span className="w-20 text-right text-[11.5px] text-muted">
                    {formatCount(page.views)} views
                  </span>
                </li>
              ))}
            </ul>
          )}
        </Panel>

        <Panel
          title="Event feed"
          subtitle="The most recent events of the site"
          bodyClassName="p-0"
          testId="realtime-events"
        >
          {snapshot.events.length === 0 ? (
            <p className="px-4 py-4 text-[12.5px] text-muted">
              No events in the last half hour — downloads, form submits and custom events appear
              here as they arrive.
            </p>
          ) : (
            <ul data-rt-events className="flex flex-col divide-y divide-line">
              {snapshot.events.map((event, index) => (
                <li key={`${event.name}-${event.occurred_at}-${index}`} className="flex items-center gap-3 px-4 py-2">
                  <span className="flex items-center gap-1.5 text-[12.5px] text-ink">
                    {event.name === "download" || event.name === "form_submit" ? (
                      <Target className="size-3.5 text-accent" aria-hidden />
                    ) : null}
                    {event.name}
                  </span>
                  <span className="min-w-0 flex-1 truncate font-mono text-[11.5px] text-muted">
                    {event.path ?? ""}
                  </span>
                  <span className="font-mono text-[11.5px] text-muted">
                    {event.value === null ? "" : event.value}
                  </span>
                  <LastSeen value={event.occurred_at} />
                </li>
              ))}
            </ul>
          )}
        </Panel>
      </div>
    </div>
  );
}

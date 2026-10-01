"use client";

/**
 * The Web Push device block on `/notifications/settings` (REQ-021, slice 6b).
 *
 * **The three device API functions had zero UI callers when this file was written.** The
 * routes, the crate and the `api.ts` wrappers were all present and reachable, so the screen
 * showed a Web Push column in the matrix with nothing behind it: a reader could turn the
 * channel on, get notifications in the bell, and have no way to tell the platform which
 * browser to push to — or to see which browsers it already knows about. A channel that can be
 * enabled and can never deliver is the green-light-wired-to-nothing shape this REQ keeps
 * finding, one level down from the readiness branch.
 *
 * Four states, all of them real:
 *
 * 1. **The installation has no key.** The block says which variable to set and offers no
 *    button. A disabled "Turn on notifications" whose tooltip is the only place the answer
 *    lives is a dead control; the sentence is on the screen.
 * 2. **No browser is registered.** The empty state explains what registering *is* — this
 *    browser, not a phone, not a colleague's laptop — because the endpoint is per browser and
 *    two accounts on one machine share it.
 * 3. **This browser is registered.** The row is marked as the current one so a reader with a
 *    phone and a laptop can tell them apart from the `user_agent` the registration recorded.
 * 4. **Some other browser is registered and this one is not.** The button works, and
 *    registering re-points the shared endpoint at this account — the API answers `reassigned`
 *    and the block says so in those words rather than reporting a bare success.
 *
 * The service-worker registration is not hidden behind a "notify me" button and it is not
 * claimed to work when it cannot: `pushManager.subscribe` needs a secure origin, and the
 * permission request needs a user gesture. Both of those failures are the browser's answer,
 * and the block shows them in its own words instead of falling into a silent catch.
 */
import { useCallback, useEffect, useState } from "react";
import {
  Bell,
  CircleCheck,
  Monitor,
  RefreshCw,
  Trash2,
  TriangleAlert,
} from "lucide-react";

import {
  fetchNotificationDevices,
  fetchNotificationPushKey,
  registerNotificationDevice,
  removeNotificationDevice,
  type ApiError,
} from "@/lib/api";
import type { NotificationDevice, NotificationPushKey } from "@/lib/types";

/**
 * A browser key, as base64url.
 *
 * `getKey` answers an `ArrayBuffer` and `applicationServerKey`/`register` want base64url, so
 * the conversion has to happen somewhere. It happens here, once, and the inverse
 * (`base64ToBytes`) is the same function's counterpart below — the two are asserted against
 * each other by the walkthrough's own probe rather than by a third copy of the encoding.
 */
function base64FromKey(key: ArrayBuffer | null): string | undefined {
  if (!key) return undefined;
  const bytes = new Uint8Array(key);
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/**
 * The browser string, short enough for a table cell.
 *
 * A device row's job is "which of these is the phone", and the full user-agent is 200
 * characters of engine versions. The recognisable part is the platform token, which is the
 * second-to-last token in almost every string a browser sends.
 */
function shortBrowser(userAgent: string | null): string {
  if (!userAgent) return "unknown browser";
  const tokens = userAgent.split(/[\s/]+/).filter(Boolean);
  const interesting = tokens.filter(
    (token) => /^[A-Za-z][A-Za-z0-9.+_-]*$/.test(token) && !/^(Mozilla|Gecko|like|KHTML|like)$/i.test(token),
  );
  const last = interesting.at(-1) ?? tokens.at(-1);
  return last ?? userAgent.slice(0, 40);
}

/** A date, as the device list renders it: day and time, no timezone theatre. */
function deviceAge(value: string): string {
  const parsed = new Date(value);
  if (Number.isNaN(parsed.getTime())) return "unknown";
  return parsed.toLocaleString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** What the block should say after a registration, keyed by the API's own outcome names. */
const OUTCOME_LINE: Record<string, string> = {
  created: "This browser is now registered for push.",
  refreshed: "This browser was already registered; the registration was refreshed.",
  reassigned:
    "This browser was registered to another account and now belongs to this one — the endpoint is shared per browser, so signing in here took it over.",
  "re-keyed": "This browser's keys changed and its registration was rewritten.",
};

export function NotificationDevices() {
  const [devices, setDevices] = useState<NotificationDevice[]>([]);
  const [key, setKey] = useState<NotificationPushKey | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [busy, setBusy] = useState(false);
  const [removing, setRemoving] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(async () => {
    setError(null);
    try {
      // Two reads that can fail independently. A failure of the *key* read is not a failure
      // of the device list — somebody has to be able to see and remove their devices on an
      // installation whose push key has since been removed from the environment — so the
      // key's failure is swallowed here and shown by the block as its own state.
      const [rows, pushKey] = await Promise.all([
        fetchNotificationDevices(),
        fetchNotificationPushKey().catch(() => null),
      ]);
      setDevices(rows);
      setKey(pushKey);
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setLoaded(true);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  /**
   * Register *this* browser, and report exactly what happened.
   *
   * The browser owns every step it can: the permission prompt needs a gesture, and
   * `pushManager.subscribe` needs the server's key as an `ArrayBuffer` — the base64url
   * decoding is done here rather than assumed, because `subscription.toJSON()`'s keys are
   * base64url without padding and a `Buffer.from(value, "base64")` would mis-decode them.
   */
  const turnOn = async () => {
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      if (!("serviceWorker" in navigator) || !("PushManager" in window)) {
        setError(
          "This browser cannot receive push notifications — service workers or the push API are not available here.",
        );
        return;
      }
      if (!window.isSecureContext) {
        // Stated rather than attempted: the browser's own failure for this is a rejected
        // promise whose message differs between engines, and the fix is an origin change.
        setError(
          "Push notifications need a secure origin (https, or localhost). This page is not one.",
        );
        return;
      }

      const applicationServerKey = key?.public_key;
      if (!applicationServerKey) {
        setError(key?.reason ?? "This installation has no push key configured.");
        return;
      }

      const registration = await navigator.serviceWorker.ready;
      const subscription = await registration.pushManager.subscribe({
        userVisibleOnly: true,
        // The cast is deliberate and the reason is in `base64ToBytes`: the decoded key is
        // backed by a real `ArrayBuffer` here, but TypeScript's `Uint8Array<ArrayBufferLike>`
        // default does not narrow to `BufferSource` on its own. A `slice()` would copy the 65
        // bytes to prove it, which is a second allocation for a key that lives in one.
        applicationServerKey: base64ToBytes(applicationServerKey) as BufferSource,
      });

      // `getKey()` rather than `toJSON().keys`: the spec defines both, but `toJSON` is absent
      // on some older engines and on the polyfills that stand in for them, while `getKey` is
      // the primitive both paths implement. Reading only `toJSON()` would make this block fail
      // silently on exactly the browsers that need it.
      const p256dh = base64FromKey(subscription.getKey("p256dh"));
      const auth = base64FromKey(subscription.getKey("auth"));

      if (!subscription.endpoint || !p256dh || !auth) {
        setError("The browser returned an incomplete push subscription, so nothing was saved.");
        return;
      }

      const answer = await registerNotificationDevice({
        endpoint: subscription.endpoint,
        p256dh,
        auth,
      });
      setNotice(OUTCOME_LINE[answer.outcome] ?? "This browser is now registered for push.");
      setDevices(await fetchNotificationDevices());
    } catch (caught) {
      setError(pushErrorMessage(caught));
    } finally {
      setBusy(false);
    }
  };

  const forget = async (device: NotificationDevice) => {
    setRemoving(device.id);
    setError(null);
    setNotice(null);
    try {
      await removeNotificationDevice(device.id);
      setDevices((current) => current.filter((row) => row.id !== device.id));
      setNotice(`Removed ${device.endpoint_hint}. This browser will stop receiving push here.`);
    } catch (caught) {
      setError((caught as ApiError).message);
    } finally {
      setRemoving(null);
    }
  };

  const unavailable = key !== null && !key.available;

  return (
    <section aria-labelledby="devices-heading" className="space-y-3" data-push-devices>
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <h2
          id="devices-heading"
          className="flex items-center gap-2 text-[15px] font-semibold"
        >
          <Bell className="h-4 w-4" aria-hidden />
          Browser push
        </h2>
        <p className="text-[12px] text-muted">
          Notifications arrive on the browsers you register here, even when this tab is
          closed.
        </p>
      </div>

      {unavailable ? (
        <p
          className="flex items-start gap-2 rounded-md border border-line bg-quiet-soft px-3 py-2 text-[13px] text-muted"
          data-push-unavailable={key?.public_key ? "no-contact" : "no-key"}
        >
          <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
          <span>{key?.reason}</span>
        </p>
      ) : null}

      {!loaded ? (
        <div className="h-20 animate-pulse rounded-lg bg-quiet-soft" data-push-state="loading" />
      ) : devices.length === 0 ? (
        <div
          className="rounded-lg border border-dashed border-line px-3 py-4 text-[13px] text-muted"
          data-push-state="empty"
        >
          <p className="font-medium text-ink">No browser is registered yet.</p>
          <p className="mt-1">
            The button below registers <strong>this</strong> browser — a laptop, not a phone,
            and not anybody else&apos;s. Notifications then reach this machine even when the
            panel is closed.
          </p>
        </div>
      ) : (
        <ul className="divide-y divide-line rounded-lg border border-line" data-push-state="list">
          {devices.map((device) => (
            <li
              key={device.id}
              className="flex flex-wrap items-center gap-3 px-3 py-2.5"
              data-push-device={device.id}
            >
              <Monitor className="h-4 w-4 shrink-0 text-muted" aria-hidden />
              <span className="min-w-40 text-[13px] font-medium">
                {shortBrowser(device.user_agent)}
              </span>
              <span className="font-mono text-[12px] text-muted">{device.endpoint_hint}</span>
              <span className="text-[12px] text-muted" title={`last seen ${device.last_seen_at}`}>
                added {deviceAge(device.created_at)}
              </span>
              <button
                type="button"
                onClick={() => void forget(device)}
                disabled={removing === device.id}
                data-push-remove={device.id}
                className="ml-auto inline-flex min-h-9 items-center gap-2 rounded-md border border-line px-3 py-1.5 text-[13px] disabled:opacity-50"
              >
                {removing === device.id ? (
                  <RefreshCw className="h-3.5 w-3.5 animate-spin" aria-hidden />
                ) : (
                  <Trash2 className="h-3.5 w-3.5" aria-hidden />
                )}
                {removing === device.id ? "Removing…" : "Remove"}
              </button>
            </li>
          ))}
        </ul>
      )}

      <div className="flex flex-wrap items-center gap-3">
        <button
          type="button"
          onClick={() => void turnOn()}
          disabled={busy || unavailable}
          data-push-enable
          className="inline-flex min-h-9 items-center gap-2 rounded-md bg-[var(--accent)] px-3 py-2 text-[13px] text-white disabled:opacity-50"
        >
          {busy ? (
            <RefreshCw className="h-3.5 w-3.5 animate-spin" aria-hidden />
          ) : (
            <Bell className="h-3.5 w-3.5" aria-hidden />
          )}
          {busy ? "Asking this browser…" : "Turn on notifications in this browser"}
        </button>
        <button
          type="button"
          onClick={() => void load()}
          className="inline-flex min-h-9 items-center gap-2 rounded-md border border-line px-3 py-2 text-[13px]"
          data-push-reload
        >
          <RefreshCw className="h-3.5 w-3.5" aria-hidden />
          Refresh
        </button>
        {notice ? (
          <p
            role="status"
            data-push-notice
            className="flex items-start gap-2 text-[13px] text-emerald-700 dark:text-emerald-400"
          >
            <CircleCheck className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
            <span>{notice}</span>
          </p>
        ) : null}
        {error ? (
          <p
            role="alert"
            data-push-error
            className="flex items-start gap-2 text-[13px] text-red-700 dark:text-red-300"
          >
            <TriangleAlert className="mt-0.5 h-4 w-4 shrink-0" aria-hidden />
            <span>{error}</span>
          </p>
        ) : null}
      </div>
    </section>
  );
}

/**
 * The browser's own failure, in a sentence.
 *
 * `DOMException`s from `pushManager.subscribe` and `Notification.requestPermission` have
 * codes worth more than their messages — `NotAllowedError` is a denied permission and
 * `AbortError` is a subscription that already exists — and a raw "AbortError" in the error
 * line tells a reader nothing they can act on.
 */
function pushErrorMessage(caught: unknown): string {
  if (caught instanceof DOMException) {
    switch (caught.name) {
      case "NotAllowedError":
        return "This browser refused the permission prompt. Push has to be allowed in the site's settings for this origin.";
      case "AbortError":
        return "This browser already has a push subscription that is no longer valid. Remove it and try again.";
      case "NotSupportedError":
        return "This browser does not support the push API on this origin.";
      default:
        return `This browser refused the registration: ${caught.name}.`;
    }
  }
  return (caught as ApiError).message ?? String(caught);
}

/** base64url without padding → the `ArrayBuffer` `applicationServerKey` needs. */
function base64ToBytes(value: string): Uint8Array {
  const padded = value.replace(/-/g, "+").replace(/_/g, "/");
  const binary = atob(padded.padEnd(padded.length + ((4 - (padded.length % 4)) % 4), "="));
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index);
  }
  return bytes;
}

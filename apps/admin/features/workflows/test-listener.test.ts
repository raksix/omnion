import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  captureView,
  describeType,
  formatDuration,
  isTriggerType,
  payloadSource,
  secondsLeft,
  startability,
  summarySentence,
  type ListenerRow,
} from "./test-listener.ts";

/**
 * The panel half of criterion 5.
 *
 * What these cases are for, concretely: the failure modes of this control are an **enabled
 * button that always answers 400** (a non-trigger node armed, refused by the server) and a
 * **panel that shows "listening" over a listener that expired** (a local counter that
 * drifts while the tab is backgrounded). Neither is visible until somebody clicks it or
 * waits, so every refusal gets a case and every countdown is asserted against a fixed
 * instant rather than a wall clock.
 */

const NOW = Date.parse("2026-09-29T12:00:00.000Z");

function at(offsetSeconds: number): string {
  return new Date(NOW + offsetSeconds * 1000).toISOString();
}

function row(over: Partial<ListenerRow> = {}): ListenerRow {
  return {
    id: "listener-1",
    node_id: "trigger",
    event_name: "page.published",
    status: "armed",
    expires_at: at(900),
    expires_in_seconds: 900,
    payload: null,
    ...over,
  };
}

const TRIGGER = { id: "trigger", type: "trigger.event", label: "New page" };
const ACTION = { id: "send", type: "action", label: "Tell the editor" };

describe("startability", () => {
  it("offers the control on a selected trigger", () => {
    const answer = startability([TRIGGER], [], NOW);
    assert.equal(answer.canListen, true);
    assert.equal(answer.nodeId, "trigger");
    assert.equal(answer.refusal, null);
    assert.equal(answer.message, null);
  });

  it("refuses with nothing selected, and says what to select", () => {
    const answer = startability(null, [], NOW);
    assert.equal(answer.canListen, false);
    assert.equal(answer.refusal, "no_node");
    // A refusal that only refuses teaches the author nothing.
    assert.match(answer.message ?? "", /select the node/i);
  });

  it("refuses a node that is not a trigger, naming it", () => {
    const answer = startability([ACTION], [], NOW);
    assert.equal(answer.canListen, false);
    assert.equal(answer.refusal, "not_a_trigger");
    // The sentence has to name the node, or the author has to guess which card to click.
    assert.match(answer.message ?? "", /Tell the editor/);
    assert.match(answer.message ?? "", /trigger node/i);
  });

  it("refuses a second arm of the same node", () => {
    const answer = startability([TRIGGER], [row()], NOW);
    assert.equal(answer.canListen, false);
    assert.equal(answer.refusal, "already_armed");
  });

  it("offers it on a *different* node while one node is already listening", () => {
    // Two armed nodes at once is the reason the listener is node-scoped; a rule-level
    // unique index would make this press silently replace the first.
    const answer = startability(
      [{ id: "second", type: "trigger.schedule", label: "Nightly" }],
      [row()],
      NOW,
    );
    assert.equal(answer.canListen, true, "an armed node must not block another");
  });

  it("offers it again once the armed row's window has closed", () => {
    // The row is still `armed` on the server until something reads it back; the panel must
    // not treat a spent window as a live listener.
    const answer = startability(
      [TRIGGER],
      [row({ expires_at: at(-1), expires_in_seconds: 0 })],
      NOW,
    );
    assert.equal(answer.canListen, true, "an expired window may be re-armed");
  });
});

describe("secondsLeft", () => {
  it("reads the expiry instant, so a ticking panel counts down", () => {
    assert.equal(secondsLeft(row({ expires_at: at(60) }), NOW), 60);
  });

  it("never exceeds the server's own number, so a stale read cannot inflate it", () => {
    // A row whose expiry is in the future but whose server-side countdown says zero is a
    // stale read; the honest answer is "nothing left", not fifteen minutes.
    assert.equal(secondsLeft(row({ expires_in_seconds: 0 }), NOW), 0);
  });

  it("clamps at zero rather than reporting a negative", () => {
    assert.equal(secondsLeft(row({ expires_at: at(-94), expires_in_seconds: 0 }), NOW), 0);
  });

  it("treats an unparseable expiry as nothing left", () => {
    assert.equal(secondsLeft(row({ expires_at: "not a date" }), NOW), 0);
  });
});

describe("summarySentence", () => {
  it("says it is not listening when nothing is armed", () => {
    assert.match(summarySentence([], NOW), /not listening/i);
  });

  it("names the event and the time left while armed", () => {
    const sentence = summarySentence([row({ expires_at: at(300) })], NOW);
    assert.match(sentence, /page\.published/);
    assert.match(sentence, /5 minutes left/);
  });

  it("counts two armed nodes, because a canvas can hold two", () => {
    const sentence = summarySentence(
      [row({ expires_at: at(300) }), row({ id: "l2", node_id: "nightly", event_name: "cron.tick" })],
      NOW,
    );
    assert.match(sentence, /and 1 more/);
  });

  it("leads with the capture when one is there", () => {
    const sentence = summarySentence(
      [row({ status: "captured", payload: { slug: "notes" } })],
      NOW,
    );
    assert.match(sentence, /captured page\.published/i);
  });

  it("says an expired listener expired, rather than saying nothing", () => {
    // The alternative — a row that quietly disappears — is indistinguishable from one
    // that was never armed, and only the second is actionable.
    const sentence = summarySentence(
      [row({ status: "expired", expires_at: at(-1), expires_in_seconds: 0 })],
      NOW,
    );
    assert.match(sentence, /expired with no event/i);
  });
});

describe("formatDuration", () => {
  it("counts in seconds under a minute", () => {
    assert.equal(formatDuration(42), "42s left");
  });

  it("counts in minutes above one, keeping the seconds", () => {
    assert.equal(formatDuration(300), "5 minutes left");
    assert.equal(formatDuration(330), "5m 30s left");
  });

  it("says so plainly at zero rather than showing -1s", () => {
    assert.equal(formatDuration(0), "no time left");
    assert.equal(formatDuration(-5), "no time left");
  });
});

describe("payloadSource", () => {
  it("prefers the capture over an armed row even when the armed row is newer", () => {
    // A rule being listened to while an older capture sits below is the normal state, and
    // `listeners[0]` would pick the armed row — the one with no payload to show.
    const source = payloadSource(
      [row({ id: "armed" }), row({ id: "captured", status: "captured", payload: { a: 1 } })],
      NOW,
    );
    assert.equal(source?.id, "captured");
  });

  it("falls back to the armed row when nothing has been captured", () => {
    assert.equal(payloadSource([row()], NOW)?.id, "listener-1");
  });

  it("returns nothing rather than an expired row", () => {
    assert.equal(
      payloadSource([row({ status: "expired", expires_at: at(-1) })], NOW),
      null,
    );
  });
});

describe("captureView", () => {
  it("calls a payload a capture — the only sentence allowed to use the word", () => {
    const view = captureView(row({ status: "captured", payload: { a: 1 } }), NOW);
    assert.equal(view.kind, "captured");
    assert.match(view.sentence, /^Captured page\.published/);
  });

  it("calls an ARMED row waiting, not captured — the defect this whole function is for", () => {
    // `status: "armed"` with `payload: null` is a listener that has heard nothing. The panel
    // used to draw a green "Captured page.published" over exactly this row, so the state the
    // feature exists to reveal was the one state it could not show.
    const view = captureView(row(), NOW);
    assert.equal(view.kind, "waiting");
    assert.equal(
      /Captured/.test(view.sentence),
      false,
      "an uncaptured listener must never be described as captured",
    );
    assert.match(view.sentence, /^Listening for page\.published/);
  });

  it("reads the armed row's countdown from the expiry instant, not a stored number", () => {
    // The panel re-renders once a second, so the sentence has to move on its own.
    const view = captureView(row({ expires_in_seconds: 900, expires_at: at(90) }), NOW);
    assert.match(view.sentence, /1m 30s left$/);
  });

  it("calls a window that closed with no payload expired, and never a capture", () => {
    // The disagreement this function exists for: a row still labelled `captured` by the
    // server, or an armed row whose 15 minutes ran out. A timeout is not a capture.
    const timedOut = captureView(row({ expires_at: at(-1) }), NOW);
    assert.equal(timedOut.kind, "expired");
    assert.equal(/Captured/.test(timedOut.sentence), false);
    assert.match(timedOut.sentence, /expired with no event/);

    // A `captured` status with no payload is the same lie arriving from the other side.
    const payloadless = captureView(row({ status: "captured", payload: null }), NOW);
    assert.equal(payloadless.kind, "waiting");
    assert.equal(/Captured/.test(payloadless.sentence), false);
  });

  it("prefers the payload over a closed window, so a late-arriving capture is still shown", () => {
    // A listener that caught the event as its window shut is the one case where the payload
    // is the truer story, and a `secondsLeft <= 0` check placed first would hide it.
    const view = captureView(
      row({ status: "captured", payload: { late: true }, expires_at: at(-30) }),
      NOW,
    );
    assert.equal(view.kind, "captured");
  });

  it("treats an unparseable expiry as nothing left rather than listening forever", () => {
    assert.equal(captureView(row({ expires_at: "not-a-date" }), NOW).kind, "expired");
  });
});

describe("isTriggerType", () => {
  it("reads the rule off the type key rather than a second list", () => {
    // A second list is a second answer to "what is a trigger", and it is the kind that
    // drifts: the registry gains a type and the panel keeps refusing it.
    assert.equal(isTriggerType("trigger.event"), true);
    assert.equal(isTriggerType("trigger.schedule"), true);
    assert.equal(isTriggerType("action"), false);
    assert.equal(isTriggerType("condition"), false);
  });
});

describe("describeType", () => {
  it("drops the family prefix so the sentence reads as a word", () => {
    assert.equal(describeType("trigger.schedule"), "schedule");
    assert.equal(describeType("action"), "action");
  });
});

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import { retryAnswer, retryMessage } from "./retry-node.ts";

/**
 * The client half of criterion 3.
 *
 * These cases are the *same* rules the server asserts in `retry_node.rs`, written down
 * separately on purpose. A copy is a liability only if the two are allowed to drift
 * silently, and the thing that stops them drifting is naming the same shapes on both sides
 * — the point of the last test here is the sentence they must both be able to produce.
 *
 * What the tests are for, concretely: the failure mode of this control is a **live button
 * that always fails** or a **dead button on a node that would have worked**, and neither is
 * visible until somebody clicks it. So every refusal gets a case, and every refusal is
 * checked for a *reason* rather than merely for `false`.
 */

describe("retryAnswer", () => {
  it("offers the button on a node that failed", () => {
    const answer = retryAnswer("Upstream is down", {
      runStatus: "failed",
      nodeStatus: "failed",
    });
    assert.equal(answer.canRetry, true);
    assert.equal(answer.reason, null);
    assert.equal(answer.code, null);
  });

  it("offers it on a cancelled step, which is what a stop policy leaves behind", () => {
    // A `stop` policy closes the tail as `cancelled`, so the last node an operator wants to
    // try again is very often a cancelled one. Refusing it would make the control useless
    // exactly when a run half-worked.
    const answer = retryAnswer("Record", {
      runStatus: "failed",
      nodeStatus: "cancelled",
    });
    assert.equal(answer.canRetry, true);
  });

  it("refuses a node that succeeded, and says so", () => {
    // The one that matters most: *Run from here* says yes on this same card, because
    // starting a new run there is a real thing to want. Retrying there is not, and a
    // button that shares the other button's answer is a button that always fails.
    const answer = retryAnswer("Tell the editor", {
      runStatus: "completed",
      nodeStatus: "succeeded",
    });
    assert.equal(answer.canRetry, false);
    assert.equal(answer.code, "nothing_to_retry");
    assert.match(answer.reason ?? "", /Tell the editor/);
    assert.match(answer.reason ?? "", /did not fail/);
  });

  it("refuses a pending step, because the run never got there", () => {
    // Re-queueing a pending step races the engine's own claim: the step is about to be
    // claimed anyway, and the write would be a no-op the operator reads as a retry.
    const answer = retryAnswer("Record", {
      runStatus: "running",
      nodeStatus: "pending",
    });
    assert.equal(answer.canRetry, false);
  });

  it("gives a node with no step its own refusal, distinct from a success", () => {
    // The two are the same answer for a `find` over the step list, and different sentences
    // for a person: "took no part in the run" is about the node, "did not fail" is about a
    // step that exists. Folding them together tells an operator debugging a click on a note
    // that the note "did not fail" — a claim about a step that was never created.
    const absent = retryAnswer("A note", {
      runStatus: "failed",
      nodeStatus: null,
    });
    const succeeded = retryAnswer("Record", {
      runStatus: "completed",
      nodeStatus: "succeeded",
    });
    assert.equal(absent.code, "node_not_in_run");
    assert.equal(succeeded.code, "nothing_to_retry");
    assert.notEqual(absent.code, succeeded.code);
    assert.match(absent.reason ?? "", /took no part/);
  });

  it("refuses a live run before it looks at the node at all", () => {
    // The run's state outranks every step-level answer. A step read from a live run
    // describes a run that is about to change underneath the click, and the server would
    // answer 409 — so a live button here is a button that fails on every press.
    const answer = retryAnswer("Upstream is down", {
      runStatus: "running",
      nodeStatus: "failed",
    });
    assert.equal(answer.canRetry, false);
    assert.equal(answer.code, "run_still_running");
  });

  it("refuses a cancelled run, and does not name the node", () => {
    // A run-level refusal is about the *run*. Naming the node would point the operator at
    // the card they clicked and away from the run that was closed on purpose — and the two
    // sentences are the difference between "this node is wrong" and "this run is over".
    const answer = retryAnswer("Upstream is down", {
      runStatus: "cancelled",
      nodeStatus: "failed",
    });
    assert.equal(answer.code, "run_cancelled");
    assert.doesNotMatch(answer.reason ?? "", /Upstream is down/);
    assert.match(answer.reason ?? "", /cancelled on purpose/);
  });

  it("refuses every node when no run has been read", () => {
    // A rule that has never run has no status layer at all. Offering the button because
    // "nothing has failed yet" is a button whose first click is always a 400.
    const answer = retryAnswer("Tell the editor", {
      runStatus: null,
      nodeStatus: null,
    });
    assert.equal(answer.canRetry, false);
  });

  it("gives every refusal a reason that says what to do", () => {
    // A disabled button with no reason teaches nothing, and a live button that always
    // fails is worse. So the reason is not optional on any refusal.
    const cases = [
      { runStatus: "running", nodeStatus: "failed" },
      { runStatus: "cancelled", nodeStatus: "failed" },
      { runStatus: "failed", nodeStatus: null },
      { runStatus: "completed", nodeStatus: "succeeded" },
      { runStatus: "failed", nodeStatus: "pending" },
    ] as const;

    for (const facts of cases) {
      const answer = retryAnswer("Some node", facts);
      assert.equal(answer.canRetry, false, `${JSON.stringify(facts)} must be refused`);
      assert.ok(
        (answer.reason ?? "").length > 15,
        `a refusal that only refuses teaches nothing: ${JSON.stringify(facts)}`,
      );
    }
  });
});

describe("retryMessage", () => {
  it("names the node, the step, and the scope", () => {
    // "Retried" is true of every press this button can produce. The scope — *only this
    // node* — is the whole distinction from the run-detail's tail re-run, so the sentence
    // has to carry it: an operator who cannot see it from the toast has no way to tell the
    // two controls apart after the fact.
    const message = retryMessage("Upstream is down", 2, 1);
    assert.match(message, /Upstream is down/);
    assert.match(message, /step 2/);
    assert.match(message, /on its own/);
  });

  it("shouts when the server re-queued more than one step", () => {
    // The server answers 1 and the walk asserts it, so a count above one is a **tail
    // retry wearing this button's name** — and it has already re-sent whatever came
    // before. Reporting a tidy success over that is the one way this control could cause
    // the exact harm the criterion forbids and still look correct.
    const message = retryMessage("Upstream is down", 2, 3);
    assert.match(message, /WARNING/);
    assert.match(message, /3 steps/);
  });
});

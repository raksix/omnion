"use client";

import { useEffect, useRef } from "react";

/**
 * Marks the document as hydrated for automated tooling.
 *
 * The QA walkthrough (`scripts/qa/walkthrough.cjs`) stamps attributes and clicks as soon as a page
 * loads. In `next dev` that lands inside React's hydration window, where React then reports a
 * hydration mismatch for the changes the pass itself made — noise that reads like a product defect.
 * The harness waits for `data-app-ready="1"` before it touches a document, so this component is the
 * signal. It carries no application behaviour: hydration itself is what flips the attribute.
 */
export function AppReady() {
  const ref = useRef<HTMLSpanElement | null>(null);

  useEffect(() => {
    ref.current?.setAttribute("data-app-ready", "1");
  }, []);

  return <span ref={ref} data-app-ready="0" hidden aria-hidden="true" />;
}

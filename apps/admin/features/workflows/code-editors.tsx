"use client";

/**
 * The CodeMirror 6 editors the inspector embeds (REQ-086 slice 3).
 *
 * Two editors live here, and they are separate components on purpose. They look like the same
 * problem and they are not:
 *
 * - {@link CodeEditor} is a **real** code editor for `ui: "code"` parameters: a language, line
 *   numbers, bracket matching and in-editor search. It owns a CodeMirror view, which is a DOM
 *   widget — so it must not be re-created when the value changes, and it must be torn down when
 *   the field it is bound to goes away (switching node, closing the inspector). Both are done
 *   here rather than by the canvas, because "when does this widget stop existing" is the only
 *   thing the widget itself can answer correctly.
 * - {@link ExpressionField} is a text field *with* an expression menu, and the interesting part
 *   is not the editing. It is that the candidate list comes from the **server**, keyed on the
 *   text inside the braces, and that the three groups the REQ names are visible while choosing.
 *
 * The rule neither of them breaks: **nothing here evaluates anything.** A preview is a server
 * answer about pinned sample data; a completion list is a server answer about the graph the
 * canvas holds. If either ever needed a client-side evaluator, it would be a second copy of the
 * grammar, and the copy the person is looking at is the one that has to be right.
 */
import { useCallback, useEffect, useRef, useState } from "react";

import { javascript } from "@codemirror/lang-javascript";
import { json } from "@codemirror/lang-json";
import { python } from "@codemirror/lang-python";
import { Compartment, EditorState, type Extension } from "@codemirror/state";
import { EditorView, keymap, lineNumbers } from "@codemirror/view";
import {
  HighlightStyle,
  bracketMatching,
  defaultHighlightStyle,
  syntaxHighlighting,
} from "@codemirror/language";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
import { search, searchKeymap } from "@codemirror/search";
// `@codemirror/lint`, not `linter` — the package is named after the thing it does and the
// export is named `linter`. Reading the diagnostics off the lint package rather than off the
// `codemirror` barrel is deliberate too: the barrel re-exports the *basic setup* extension,
// which brings its own keymap and history, and two histories in one editor is an undo that
// silently does nothing.
import { linter, type Diagnostic } from "@codemirror/lint";

import { completeExpressions } from "@/lib/api";
import { ApiError } from "@/lib/api";
import type {
  CompletionCandidate,
  CompletionSource,
  GraphDocument,
  GraphIssue,
} from "@/lib/types";

/** The languages the registry's code parameters are written in. */
export type CodeLanguage = "javascript" | "python" | "json";

/**
 * The language for a code parameter, read from the placeholder rather than a new field.
 *
 * The registry's `NodeParam` carries `ui`, `placeholder` and `help` and nothing else, and this
 * is the moment where adding a `language` to it would be a change to a schema eight other
 * screens read. A placeholder that *names* a language is an unambiguous signal a person can
 * also read; an unrecognised one falls back to plain text rather than to a wrong grammar,
 * because highlighting JavaScript as Python is worse than not highlighting at all.
 */
export function languageFor(placeholder: string | null | undefined): CodeLanguage | null {
  const text = (placeholder ?? "").toLowerCase();
  if (/\bjavascript\b|\bjs\b/.test(text)) return "javascript";
  if (/\bpython\b|\bpy\b/.test(text)) return "python";
  if (/\bjson\b/.test(text)) return "json";
  return null;
}

/** The CodeMirror extension set for one language, or none when it is not a known language. */
function languageExtension(language: CodeLanguage | null): Extension[] {
  switch (language) {
    case "javascript":
      return [javascript()];
    case "python":
      return [python()];
    case "json":
      return [json()];
    default:
      return [];
  }
}

/** The palette, so code and the panel around it are the same dark or the same light. */
const THEME = EditorView.theme(
  {
    "&": {
      fontSize: "12px",
      border: "1px solid var(--color-line, #d4d4d8)",
      borderRadius: "6px",
      backgroundColor: "var(--color-background, #fff)",
      color: "var(--color-foreground, #18181b)",
    },
    ".cm-content": { fontFamily: "ui-monospace, monospace", minHeight: "90px" },
    ".cm-gutters": {
      backgroundColor: "transparent",
      border: "none",
      color: "var(--color-muted, #71717a)",
    },
    ".cm-activeLine": { backgroundColor: "rgba(120,120,140,0.08)" },
    "&.cm-focused": { outline: "2px solid var(--color-ring, #6366f1)" },
  },
  { dark: false },
);

/**
 * Turn this node's validation issues into CodeMirror diagnostics.
 *
 * The mapping is line **0** for every one of them, and that is not laziness — it is honest.
 * The server validates a parameter's *value*, not its contents: `"url" must be a URL` is a
 * statement about the whole field, and inventing a line number for it would put a red mark on
 * a line the person never got wrong. When REQ-088 gives the ports their kinds and a node can
 * report a position, this is the one function that has to change.
 *
 * Reported through CodeMirror's own diagnostic pipeline rather than a decorative strip, so the
 * list is navigable: a person can jump between marks with the standard gutter controls.
 */
function diagnosticsFor(issues: GraphIssue[], field: string): Diagnostic[] {
  return issues
    .filter((issue) => issue.param === field)
    .map((issue) => ({
      from: 0,
      to: 0,
      severity: "error" as const,
      message: issue.message,
    }));
}

/**
 * A CodeMirror 6 editor for one code parameter.
 *
 * The `value` is a *seed*, not a controlled prop, and the reason is the one thing this
 * component exists to get right. CodeMirror owns its document; re-creating the view whenever
 * `value` changes would reset the cursor and the undo stack on every keystroke, which turns
 * `⌘Z` into "lose what I just typed". So the view is created once per `field`, and later
 * changes to `value` are pushed in only when they *differ* from what the editor already has —
 * which is the undo case, and the case where the inspector is showing a different node.
 */
export function CodeEditor({
  field,
  language,
  value,
  readOnly,
  issues,
  onChange,
}: {
  field: string;
  language: CodeLanguage | null;
  value: string;
  readOnly: boolean;
  issues: GraphIssue[];
  onChange: (value: string) => void;
}) {
  const host = useRef<HTMLDivElement | null>(null);
  const view = useRef<EditorView | null>(null);
  const editableRef = useRef<Compartment | null>(null);
  // Shared with the mount-only effect through a ref, so a new `issues` array does not
  // rebuild the extension. Declared here rather than inside the effect because the linter
  // callback closes over it and the effect may not run again for the life of the field.
  const diagnosticsRef = useRef<Diagnostic[]>([]);
  const change = useRef(onChange);
  change.current = onChange;

  // Mount once. `field` is in the dependency list on purpose: switching node must produce a
  // *new* editor showing the new parameter, not the old one's document with a new label.
  useEffect(() => {
    if (!host.current) return undefined;
    // Declared BEFORE the linter that reads it. The order here is not style: a
    // `const` read on the previous line is a temporal-dead-zone error at runtime, and a
    // build that is green until the editor mounts is the worst possible time to find out.
    diagnosticsRef.current = diagnosticsFor(issuesRef.current, fieldRef.current);
    const diagnostics = linter(() => diagnosticsRef.current, { delay: 200 });
    // A compartment rather than re-creating the view: `readOnly` flips when the inspector
    // opens a second editor of the same workflow, and re-creating would throw away the undo
    // history of a field somebody is halfway through.
    const editable = new Compartment();
    const created = new EditorView({
      state: EditorState.create({
        doc: valueRef.current,
        extensions: [
          lineNumbers(),
          history(),
          bracketMatching(),
          closeBrackets(),
          search({ top: true }),
          keymap.of([
            ...closeBracketsKeymap,
            ...defaultKeymap,
            ...historyKeymap,
            ...searchKeymap,
          ]),
          syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
          ...languageExtension(languageRef.current),
          EditorView.lineWrapping,
          editable.of([
            EditorState.readOnly.of(readOnlyRef.current),
            EditorView.editable.of(!readOnlyRef.current),
          ]),
          diagnostics,
          THEME,
          EditorView.updateListener.of((update) => {
            if (update.docChanged) change.current(update.state.doc.toString());
          }),
        ],
      }),
      parent: host.current,
    });
    view.current = created;
    editableRef.current = editable;
    return () => {
      created.destroy();
      view.current = null;
      editableRef.current = null;
    };
    // Intentionally mount-only: see the doc comment.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [field]);

  // Refs for the values the mount-only effect reads, so a later render does not have to
  // re-create the view to see them.
  const valueRef = useRef(value);
  valueRef.current = value;
  const issuesRef = useRef(issues);
  issuesRef.current = issues;
  const fieldRef = useRef(field);
  fieldRef.current = field;
  const languageRef = useRef(language);
  languageRef.current = language;
  const readOnlyRef = useRef(readOnly);
  readOnlyRef.current = readOnly;

  // Push a value the editor does not already have — the undo case, and the "a different node
  // is being inspected" case, which the mount effect above cannot see because `field` did not
  // change identity if the two nodes happen to share a parameter name.
  useEffect(() => {
    const current = view.current;
    if (!current) return;
    const text = current.state.doc.toString();
    if (text === value) return;
    current.dispatch({
      changes: { from: 0, to: text.length, insert: value },
      selection: { anchor: Math.min(current.state.selection.main.anchor, value.length) },
    });
  }, [value]);

  useEffect(() => {
    const current = view.current;
    const editable = editableRef.current;
    if (!current || !editable) return;
    current.dispatch({
      effects: editable.reconfigure([
        EditorState.readOnly.of(readOnly),
        EditorView.editable.of(!readOnly),
      ]),
    });
  }, [readOnly]);

  // The diagnostics are pushed rather than re-created: `linter` closes over its callback, so
  // rebuilding the extension on every validation round trip would reset the editor's
  // diagnostic state mid-edit and make the gutter flicker between marks and no marks.
  useEffect(() => {
    diagnosticsRef.current = diagnosticsFor(issues, field);
  }, [issues, field]);

  // The linter is fed through a ref so a new `issues` array does not rebuild the extension:
  // `linter()` closes over the callback, and re-creating it on every validation round trip
  // would drop the editor's diagnostic state mid-edit.
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);

  return (
    <div className="mt-1">
      <div
        ref={host}
        data-code-editor={field}
        data-code-language={language ?? "plain"}
        data-code-mounted={mounted ? "true" : "false"}
        data-code-readonly={readOnly ? "true" : "false"}
        data-code-diagnostics={diagnosticsFor(issues, field).length}
        className="overflow-hidden rounded"
      />
      {language ? (
        <p className="mt-0.5 text-[10px] text-muted">
          {language} · ⌘F searches inside the editor
        </p>
      ) : null}
    </div>
  );
}

/** How long to wait after a keystroke before asking the server for candidates. */
const COMPLETE_DEBOUNCE_MS = 180;

/**
 * What the menu is showing right now.
 *
 * `null` for the menu itself being closed is different from an empty list being shown: "the
 * menu is not open" is the answer after Escape, while "no candidate matches" is the answer
 * three characters into a namespace name. Collapsing the two makes the field look broken
 * exactly when the person needs it most.
 */
type MenuState = {
  open: boolean;
  candidates: CompletionCandidate[];
  active: number;
  /** Set when the request could not be reached or refused. */
  error: string | null;
};

const CLOSED: MenuState = { open: false, candidates: [], active: 0, error: null };

/**
 * The text inside the braces of the expression the cursor is in, or null when the cursor is
 * not inside one.
 *
 * Returns the span as well, because accepting a candidate has to *replace* exactly those
 * characters — and the naive version (split on `{{`, take the tail) corrupts a field carrying
 * two expressions, which is a shape the preview half explicitly supports.
 */
export function expressionAt(
  text: string,
  cursor: number,
): { start: number; end: number; path: string } | null {
  const open = text.lastIndexOf("{{", cursor);
  if (open < 0) return null;
  const close = text.indexOf("}}", open + 2);
  if (close < 0 || cursor > close + 2) return null;
  return {
    start: open + 2,
    end: close,
    path: text.slice(open + 2, close),
  };
}

/** One menu group, in the order the REQ names them and the server returns them. */
const GROUP_LABEL: Record<CompletionSource, string> = {
  runtime: "Run-time values",
  sample: "Pinned sample",
  upstream: "Upstream nodes",
};

/**
 * The expression field with the server's candidate menu.
 *
 * The menu opens on typing, on focus and on ArrowDown, closes on Escape and on a choice, and
 * is navigable with ArrowUp/ArrowDown/Enter/Tab — a completion list nobody can reach from the
 * keyboard is a completion list that does not exist for anybody using a keyboard.
 *
 * Only **one** request is in flight, and a stale answer is dropped rather than rendered. Both
 * are the same rule the preview follows: an answer that arrives after the text it was asked
 * about is a lie wearing a spinner, and the person reading it has no way to tell.
 */
export function ExpressionField({
  field,
  value,
  readOnly,
  workflowId,
  nodeKey,
  graph,
  namespaces,
  issues,
  onChange,
}: {
  field: string;
  value: string;
  readOnly: boolean;
  workflowId: string;
  nodeKey: string;
  graph: GraphDocument;
  namespaces: Record<string, unknown>;
  issues: GraphIssue[];
  onChange: (value: string) => void;
}) {
  const [text, setText] = useState(value);
  const [menu, setMenu] = useState<MenuState>(CLOSED);
  const input = useRef<HTMLInputElement | null>(null);
  const asking = useRef(false);
  const generation = useRef(0);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  // The inspector shows a different node's parameters without remounting this component, so
  // the field has to follow the prop rather than keep what it was seeded with.
  useEffect(() => setText(value), [value]);

  const ask = useCallback(
    async (path: string) => {
      if (asking.current) return;
      asking.current = true;
      const mine = ++generation.current;
      try {
        const answer = await completeExpressions(
          workflowId,
          nodeKey,
          path,
          graph,
          namespaces,
        );
        // A newer keystroke has already asked its own question; this answer describes text
        // that is no longer on screen.
        if (mine !== generation.current) return;
        setMenu({ open: true, candidates: answer.candidates, active: 0, error: null });
      } catch (error) {
        if (mine !== generation.current) return;
        setMenu({
          open: true,
          candidates: [],
          active: 0,
          error:
            error instanceof ApiError
              ? error.message
              : "the completion list could not be reached",
        });
      } finally {
        if (mine === generation.current) asking.current = false;
      }
    },
    [graph, namespaces, nodeKey, workflowId],
  );

  /** Read the expression at the caret and ask about it. */
  const refresh = useCallback(
    (at: number) => {
      const span = expressionAt(text, at);
      if (!span) {
        setMenu(CLOSED);
        return;
      }
      void ask(span.path);
    },
    [ask, text],
  );

  const schedule = useCallback(
    (at: number) => {
      if (timer.current) clearTimeout(timer.current);
      timer.current = setTimeout(() => refresh(at), COMPLETE_DEBOUNCE_MS);
    },
    [refresh],
  );

  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current);
    },
    [],
  );

  /** Put a candidate in place, replacing exactly the characters it stands for. */
  const accept = useCallback(
    (candidate: CompletionCandidate) => {
      const caret = input.current?.selectionStart ?? text.length;
      const span = expressionAt(text, caret);
      if (!span) {
        setMenu(CLOSED);
        return;
      }
      const next = `${text.slice(0, span.start)}${candidate.label}${text.slice(span.end)}`;
      setText(next);
      onChange(next);
      setMenu(CLOSED);
      // The caret goes after what was just inserted, so typing `{{$va` and choosing `$vars`
      // leaves the cursor able to type `site` rather than jumping back to the start.
      const caretAt = span.start + candidate.label.length;
      requestAnimationFrame(() => {
        input.current?.focus();
        input.current?.setSelectionRange(caretAt, caretAt);
      });
    },
    [onChange, text],
  );

  const move = useCallback(
    (delta: number) => {
      setMenu((current) => {
        if (!current.open || current.candidates.length === 0) return current;
        const count = current.candidates.length;
        // Wraps. A menu whose highlight sticks at the last entry makes ArrowDown look broken
        // once the list is longer than the screen.
        const active = (current.active + delta + count) % count;
        return { ...current, active };
      });
    },
    [],
  );

  const onKeyDown = useCallback(
    (event: React.KeyboardEvent<HTMLInputElement>) => {
      const { open, candidates, active } = menu;
      if (event.key === "Escape") {
        setMenu(CLOSED);
        return;
      }
      if (event.key === "ArrowDown") {
        // Opens on demand as well as navigating: focus-and-arrow is the fastest way to see
        // what a field can hold, and it is the path a keyboard user has.
        event.preventDefault();
        if (!open) {
          refresh(event.currentTarget.selectionStart ?? text.length);
          return;
        }
        move(1);
        return;
      }
      if (event.key === "ArrowUp") {
        event.preventDefault();
        if (open) move(-1);
        return;
      }
      if ((event.key === "Enter" || event.key === "Tab") && open && candidates[active]) {
        // Tab inserts rather than moving focus only when the menu is actually offering
        // something; otherwise the field must behave like every other input on the panel.
        event.preventDefault();
        accept(candidates[active]);
        return;
      }
      if (event.key === "Enter" && open) {
        // An open menu with nothing in it must not swallow the key: the person pressing
        // Enter to submit the node deserves that to work.
        setMenu(CLOSED);
      }
    },
    [accept, menu, move, refresh, text],
  );

  // Grouped for display, preserving the server's order within each group.
  const groups = menu.candidates.reduce<Record<string, CompletionCandidate[]>>((by, item) => {
    const bucket = by[item.source] ?? [];
    bucket.push(item);
    by[item.source] = bucket;
    return by;
  }, {});

  return (
    <div className="relative">
      <input
        ref={input}
        id={field}
        type="text"
        value={text}
        readOnly={readOnly}
        placeholder="{{ namespace.field }}"
        onChange={(event) => {
          const next = event.target.value;
          setText(next);
          onChange(next);
          schedule(event.target.selectionStart ?? next.length);
        }}
        onFocus={(event) => refresh(event.target.selectionStart ?? text.length)}
        onBlur={() => setTimeout(() => setMenu(CLOSED), 120)}
        onKeyDown={onKeyDown}
        data-expression-field={field}
        aria-expanded={menu.open}
        aria-autocomplete="list"
        className="mt-1 w-full rounded border border-line bg-background px-2 py-1 font-mono text-[11px]"
      />

      {menu.open ? (
        <div
          data-completions={field}
          data-candidate-count={menu.candidates.length}
          className="absolute z-30 mt-1 max-h-56 w-full overflow-auto rounded border border-line bg-background shadow-lg"
        >
          {menu.error ? (
            <p className="px-2 py-1 text-[10px] text-amber-600">{menu.error}</p>
          ) : menu.candidates.length === 0 ? (
            <p className="px-2 py-1 text-[10px] text-muted">
              No candidate matches what has been typed.
            </p>
          ) : (
            Object.entries(GROUP_LABEL).map(([source, label]) => {
              const items = groups[source];
              if (!items || items.length === 0) return null;
              return (
                <div key={source} data-completion-group={source}>
                  <p className="px-2 pt-1.5 text-[9px] tracking-wide text-muted uppercase">
                    {label}
                  </p>
                  {items.map((candidate) => {
                    const index = menu.candidates.indexOf(candidate);
                    return (
                      <button
                        key={`${candidate.source}:${candidate.label}`}
                        type="button"
                        data-candidate={candidate.label}
                        data-candidate-source={candidate.source}
                        data-candidate-active={index === menu.active ? "true" : "false"}
                        onMouseEnter={() =>
                          setMenu((current) => ({ ...current, active: index }))
                        }
                        onMouseDown={(event) => {
                          // mousedown, not click: the input's blur would close the menu before
                          // the click landed, and a completion you cannot click is a
                          // completion that only works if you guessed the keyboard shortcut.
                          event.preventDefault();
                          accept(candidate);
                        }}
                        className="flex w-full items-baseline justify-between gap-2 px-2 py-1 text-left hover:bg-muted/60"
                      >
                        <span className="font-mono text-[11px]">{candidate.label}</span>
                        <span className="truncate text-[9px] text-muted">{candidate.detail}</span>
                      </button>
                    );
                  })}
                </div>
              );
            })
          )}
        </div>
      ) : null}

      {diagnosticsFor(issues, field).length > 0 ? (
        <ul className="mt-1 space-y-0.5">
          {diagnosticsFor(issues, field).map((diagnostic) => (
            <li key={diagnostic.message} className="text-[10px] text-amber-600">
              {diagnostic.message}
            </li>
          ))}
        </ul>
      ) : null}
    </div>
  );
}

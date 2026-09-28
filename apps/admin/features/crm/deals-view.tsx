"use client";

/**
 * The pipeline board, the deals list and the deal form (docs/requests/REQ-051, slice 3).
 *
 * What this screen has to do, and why each piece exists:
 *
 * * **The board reads in one request.** Stages, cards and per-column totals arrive together, so
 *   a header and the cards beneath it can never come from two different moments — the thing
 *   that makes a kanban believable.
 * * **A drag is optimistic and reversible, and so is the keyboard.** A card paints into the new
 *   column immediately, the write follows, and a refusal puts the card back where it was with a
 *   message. `ctrl/cmd + ←/→` sends the *same* request the drag does, so the feature is not
 *   mouse-only: a drag-only control is a dead control for anyone who does not drag.
 * * **The two outcome columns are dialogs, not columns you drop into.** A lost deal must say
 *   why (the API refuses it otherwise, and a board that quietly accepted a reasonless loss
 *   would make the loss report meaningless); a won deal confirms its close date.
 * * **Stale is visible.** A card that has sat in one stage past 30 days is drawn amber, because
 *   "the deal has not moved" is the single most useful thing a pipeline can tell you.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { AlertTriangle, ArrowLeftRight, LayoutGrid, List, Plus, Sparkles, X } from "lucide-react";
import { useSearchParams } from "next/navigation";

import { EmptyState } from "@/components/empty-state";
import { ErrorStrip, toScreenError, type ScreenErrorValue } from "@/components/error-state";
import { ApiError } from "@/lib/api";
import {
  CRM_CURRENCIES,
  askCrmCopilot,
  archiveCrmDeal,
  createCrmDeal,
  fetchCrmDeals,
  fetchCrmDealsBoard,
  fetchCrmPipelines,
  moveCrmDealStage,
  updateCrmDeal,
  type CrmBoard,
  type CrmCopilotAction,
  type CrmCopilotAnswer,
  type CrmDeal,
  type CrmPipeline,
} from "@/lib/crm";

import { CrmAvatar, CrmShell, CrmTag } from "./crm-parts";
import { useCrmTenant } from "./crm-tenant";

/** What the deal form holds while it is open. */
type DealForm = {
  id: string | null;
  title: string;
  stage_id: string;
  company_id: string;
  contact_id: string;
  amount: string;
  currency: string;
  probability: string;
  expected_close_on: string;
  source: string;
  notes: string;
};

const EMPTY_FORM: DealForm = {
  id: null,
  title: "",
  stage_id: "",
  company_id: "",
  contact_id: "",
  amount: "",
  currency: "USD",
  probability: "",
  expected_close_on: "",
  source: "",
  notes: "",
};

/** The dialog a drop into an outcome column opens. */
type OutcomeDialog = {
  kind: "won" | "lost";
  deal: CrmDeal;
  stage_id: string;
  reason: string;
  close_on: string;
};

/** A money value, grouped so a column of amounts is readable at a glance. */
function money(amount: string, currency: string): string {
  const value = Number(amount ?? "0");
  if (!Number.isFinite(value)) return `${currency} 0`;
  return new Intl.NumberFormat("en-US", {
    style: "currency",
    currency,
    maximumFractionDigits: value >= 1000 ? 0 : 2,
  }).format(value);
}

/** The same grouping, for a column header, which may sum several currencies' worth of numbers. */
function totalLabel(amount: string): string {
  const value = Number(amount ?? "0");
  if (!Number.isFinite(value)) return "0";
  return new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 }).format(value);
}

/** How long a card has sat in its stage, in the words a person would use. */
function age(days: number): string {
  if (days <= 0) return "today";
  if (days === 1) return "1 day";
  if (days < 30) return `${days} days`;
  const months = Math.round(days / 30);
  return `${months} month${months === 1 ? "" : "s"}`;
}

/** The deals screen: the board, the list toggle and the form. */
export function DealsView() {
  const searchParams = useSearchParams();
  const [board, setBoard] = useState<CrmBoard | null>(null);
  const [list, setList] = useState<CrmDeal[] | null>(null);
  const [pipelines, setPipelines] = useState<CrmPipeline[]>([]);
  const [mode, setMode] = useState<"board" | "list">("board");
  const [pipelineId, setPipelineId] = useState<string>("");
  const [search, setSearch] = useState("");
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [reloadToken, setReloadToken] = useState(0);

  const [form, setForm] = useState<DealForm | null>(null);
  const [fieldError, setFieldError] = useState<{ field: string; message: string } | null>(null);
  const [saving, setSaving] = useState(false);
  const [outcome, setOutcome] = useState<OutcomeDialog | null>(null);
  const [dragId, setDragId] = useState<string | null>(null);
  const [dropStage, setDropStage] = useState<string | null>(null);
  const [focusedCard, setFocusedCard] = useState<string | null>(null);
  const titleRef = useRef<HTMLInputElement | null>(null);

  // ---- the copilot card ---------------------------------------------------------------------
  //
  // It hangs off the focused card rather than off a row, because a card *is* the record on this
  // screen: a deal with six copilot buttons and a list of forty deals is a screen where every
  // button has to ask which row it is about. One card, one deal, and it opens on the same
  // `?focus=` link a search hit uses, so the two entry points behave identically.
  const [copilotOpen, setCopilotOpen] = useState(false);
  const [copilotBusy, setCopilotBusy] = useState<CrmCopilotAction | null>(null);
  const [copilotAnswer, setCopilotAnswer] = useState<CrmCopilotAnswer | null>(null);
  const [copilotError, setCopilotError] = useState<string | null>(null);
  const copilotClose = useRef<HTMLButtonElement | null>(null);

  const runCopilot = useCallback(
    async (deal: CrmDeal, action: CrmCopilotAction) => {
      setCopilotBusy(action);
      setCopilotError(null);
      try {
        setCopilotAnswer(await askCrmCopilot(deal.id, action));
      } catch (problem) {
        // A failure is a *result* the card has to be able to show: this installation may connect
        // no provider, and "the copilot could not answer" is a sentence a person can act on where
        // a silently missing card is not.
        setCopilotError(
          problem instanceof ApiError
            ? problem.message
            : "The copilot could not be reached.",
        );
        setCopilotAnswer(null);
      } finally {
        setCopilotBusy(null);
      }
    },
    [],
  );

  // Closing with Escape is what makes a side panel feel like a panel rather than a new page.
  useEffect(() => {
    if (!copilotOpen) return;
    copilotClose.current?.focus();
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") setCopilotOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [copilotOpen]);

  // The deal the panel is about, read from the board *and* the list. The panel follows the
  // focus rather than the other way round: clicking a card sets both, and if the board reloads
  // while the panel is open the id survives in `focusedCard` even if the object does not.
  const copilotDeal = useMemo(() => {
    if (!focusedCard) return null;
    return (
      board?.deals.find((deal) => deal.id === focusedCard) ??
      list?.find((deal) => deal.id === focusedCard) ??
      null
    );
  }, [board, focusedCard, list]);

  const stages = board?.pipeline.stages ?? [];

  // The organization the panel is reading (REQ-051): a platform account has no primary one.
  const { organizationId } = useCrmTenant();

  /** Load whichever view is on screen. The board is the default because it is the screen. */
  const load = useCallback(async () => {
    setError(null);
    try {
      if (mode === "board") {
        // The route answers `{ view, board }` so a list mode can share the path; the screen
        // wants the board itself, and unwrapping it here keeps that envelope in one place.
        const response = await fetchCrmDealsBoard(pipelineId || undefined, organizationId ?? undefined);
        setBoard(response.board);
        setList(null);
      } else {
        const response = await fetchCrmDeals(
          search || organizationId ? { search: search || undefined, organization_id: organizationId ?? undefined } : {},
        );
        setList(response.page.items);
        setBoard(null);
      }
    } catch (problem) {
      setError(toScreenError(problem, "The board could not be loaded."));
    }
  }, [mode, pipelineId, search, organizationId]);

  useEffect(() => {
    void load();
  }, [load, reloadToken]);

  useEffect(() => {
    fetchCrmPipelines(organizationId ?? undefined)
      .then((rows) => {
        setPipelines(rows);
        setPipelineId((current) => current || (rows[0]?.id ?? ""));
      })
      .catch(() => {
        /* The board reports its own problem; the selector is a convenience. */
      });
  }, []);

  // ---- the move, from whichever hand sent it ----------------------------------------------

  /**
   * Move a card, or ask for what the target column needs first.
   *
   * An open column needs nothing, so it goes straight through; an outcome column opens its
   * dialog and this returns without writing. The return value is the boolean "did the write
   * happen", which is what the keyboard path needs to keep the selection where it is.
   */
  const requestMove = useCallback(
    async (deal: CrmDeal, stageId: string): Promise<boolean> => {
      const stage = stages.find((entry) => entry.id === stageId);
      if (!stage) return false;

      if (stage.kind === "lost" || stage.kind === "won") {
        setOutcome({
          kind: stage.kind,
          deal,
          stage_id: stageId,
          reason: "",
          close_on: new Date().toISOString().slice(0, 10),
        });
        return false;
      }

      const before = board;
      // Optimistic: the card is in the new column before the request goes out, and `before` is
      // what a refusal restores.
      setBoard((current) => (current ? relocate(current, deal.id, stageId) : current));
      try {
        await moveCrmDealStage(deal.id, stageId);
        setNotice(`${deal.title} moved to ${stage.name}.`);
        setReloadToken((token) => token + 1);
      } catch (problem) {
        setBoard(before);
        setError(toScreenError(problem, "The move was refused."));
      }
      return true;
    },
    [board, stages],
  );

  const confirmOutcome = useCallback(async () => {
    if (!outcome) return;
    const before = board;
    setBoard((current) => (current ? relocate(current, outcome.deal.id, outcome.stage_id) : current));
    try {
      await moveCrmDealStage(
        outcome.deal.id,
        outcome.stage_id,
        outcome.kind === "lost"
          ? { lostReason: outcome.reason.trim() }
          : { closeOn: outcome.close_on },
      );
      setNotice(`${outcome.deal.title} marked ${outcome.kind}.`);
      setOutcome(null);
      setReloadToken((token) => token + 1);
    } catch (problem) {
      setBoard(before);
      setError(toScreenError(problem, "The move was refused."));
    }
  }, [outcome, board]);

  // ---- the keyboard path --------------------------------------------------------------------

  /**
   * `ctrl/cmd + ←/→` moves the focused card one column.
   *
   * The modifier is what makes it safe: plain arrow keys belong to the page (and to the focus
   * ring), and a board where a stray arrow keypress moved a deal would be a board people
   * stopped trusting.
   */
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
      if (!event.metaKey && !event.ctrlKey) return;
      if (!focusedCard) return;

      const target = event.target as HTMLElement | null;
      if (target && (target.tagName === "INPUT" || target.tagName === "TEXTAREA" || target.tagName === "SELECT")) {
        return;
      }
      if (!board) return;

      const deal = board.deals.find((entry) => entry.id === focusedCard);
      if (!deal) return;
      const currentIndex = stages.findIndex((stage) => stage.id === deal.stage_id);
      if (currentIndex < 0) return;
      const delta = event.key === "ArrowRight" ? 1 : -1;
      const next = stages[currentIndex + delta];
      if (!next) return;

      event.preventDefault();
      setFocusedCard(next.id);
      void requestMove(deal, next.id);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [board, focusedCard, requestMove, stages]);

  // `/crm/deals?focus=<id>` — a search hit (or a shared link) marks that card and brings it into
  // view. The board is the one CRM screen where opening a *form* would be wrong: the card itself
  // is the record, and marking it is what the board's own keyboard path acts on. The applied id
  // is remembered, so the next render does not steal focus back from whoever clicked a card.
  const focusParam = searchParams.get("focus");
  const appliedFocus = useRef<string | null>(null);
  useEffect(() => {
    if (!focusParam || !board || appliedFocus.current === focusParam) {
      return;
    }
    if (!board.deals.some((deal) => deal.id === focusParam)) {
      return;
    }
    appliedFocus.current = focusParam;
    setFocusedCard(focusParam);
    // `?focus=` with `&copilot=1` is a link *to the answer*, not only to the card: the palette
    // offers "ask the copilot about this deal" as a row, and a link that lands on a card whose
    // panel stays shut makes that row a dead button. The id alone still lands on the card.
    if (searchParams.get("copilot") === "1") {
      setCopilotAnswer(null);
      setCopilotError(null);
      setCopilotOpen(true);
    }
    const card = document.querySelector<HTMLElement>(`[data-qa-card="${focusParam}"]`);
    card?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [focusParam, searchParams, board]);

  const openCreate = useCallback(() => {
    setForm({ ...EMPTY_FORM, stage_id: stages[0]?.id ?? "" });
    setFieldError(null);
  }, [stages]);

  const saveForm = useCallback(async () => {
    if (!form) return;
    setSaving(true);
    setFieldError(null);
    try {
      const payload = {
        title: form.title.trim(),
        stage_id: form.stage_id || undefined,
        company_id: form.company_id || undefined,
        contact_id: form.contact_id || undefined,
        amount: form.amount.trim() || undefined,
        currency: form.currency,
        probability: form.probability ? Number(form.probability) : undefined,
        expected_close_on: form.expected_close_on || undefined,
        source: form.source.trim() || undefined,
      };
      if (form.id) {
        await updateCrmDeal(form.id, payload);
        setNotice(`${payload.title} updated.`);
      } else {
        await createCrmDeal({ ...payload, pipeline_id: pipelineId || undefined });
        setNotice(`${payload.title} created.`);
      }
      setForm(null);
      setReloadToken((token) => token + 1);
    } catch (problem) {
      if (problem instanceof ApiError) {
        // The API names the field in `error.details.field`; the form renders the sentence under
        // the input that caused it, which is the only way a person can tell which one.
        const field = (problem as ApiError & { field?: string }).field;
        setFieldError({ field: field ?? "title", message: problem.message });
      } else {
        setFieldError({ field: "title", message: "The deal could not be saved." });
      }
    } finally {
      setSaving(false);
    }
  }, [form, pipelineId]);

  const deals = board?.deals ?? [];
  const openTotal = board?.open_total ?? "0";
  const forecast = board?.weighted_forecast ?? "0";

  return (
    <CrmShell
      title="Deals"
      description="The pipeline: a card per opportunity, a column per stage, and the weighted forecast of what is still open"
      entity="deals"
      availableColumns={[
        "title",
        "company",
        "stage",
        "owner",
        "amount",
        "probability",
        "expected_close_on",
        "updated_at",
      ]}
      statuses={stages.map((stage) => stage.kind)}
      total={deals.length}
      nextCursor={null}
      loadingMore={false}
      loadMore={() => {}}
      onCreate={openCreate}
      rowIds={deals.map((deal) => deal.id)}
      keyboard={{ onEdit: () => {}, onOpen: () => {} }}
      toolbarExtra={
        <div className="flex flex-wrap items-center gap-2">
          <label className="flex items-center gap-1.5">
            <span className="sr-only">Pipeline</span>
            <select
              id="crm-pipeline"
              value={pipelineId}
              onChange={(event) => setPipelineId(event.target.value)}
              className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12px] outline-none focus:border-accent"
            >
              {pipelines.length === 0 ? <option value="">No pipeline</option> : null}
              {pipelines.map((pipeline) => (
                <option key={pipeline.id} value={pipeline.id}>
                  {pipeline.name}
                </option>
              ))}
            </select>
          </label>

          <div className="flex overflow-hidden rounded-lg border border-line">
            <button
              type="button"
              id="crm-board-toggle"
              onClick={() => setMode("board")}
              aria-pressed={mode === "board"}
              className={`flex items-center gap-1.5 px-2.5 py-1.5 text-[12px] transition ${
                mode === "board" ? "bg-accent-soft text-accent-strong" : "text-muted hover:bg-canvas"
              }`}
            >
              <LayoutGrid className="size-3.5" aria-hidden /> Board
            </button>
            <button
              type="button"
              id="crm-list-toggle"
              onClick={() => setMode("list")}
              aria-pressed={mode === "list"}
              className={`flex items-center gap-1.5 px-2.5 py-1.5 text-[12px] transition ${
                mode === "list" ? "bg-accent-soft text-accent-strong" : "text-muted hover:bg-canvas"
              }`}
            >
              <List className="size-3.5" aria-hidden /> List
            </button>
          </div>
        </div>
      }
    >
      {error ? (
        <ErrorStrip
          error={error}
          onRetry={() => setReloadToken((token) => token + 1)}
          qa="crm-deals-error"
        />
      ) : null}
      {notice ? (
        <p className="border-b border-line bg-canvas px-4 py-2.5 text-[12px] text-muted">{notice}</p>
      ) : null}

      {mode === "list" ? (
        <label className="flex items-center gap-1.5 border-b border-line px-4 py-2.5">
          <span className="sr-only">Search deals</span>
          <input
            id="crm-deal-search"
            type="search"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
            placeholder="Search title, company or contact…"
            className="w-64 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12px] outline-none focus:border-accent"
          />
        </label>
      ) : null}

      {mode === "board" ? (
        board === null ? (
          <div className="flex gap-3 overflow-x-auto p-4" aria-busy="true">
            {[0, 1, 2, 3].map((column) => (
              <div key={column} className="h-40 w-56 shrink-0 animate-pulse rounded-xl bg-canvas" />
            ))}
          </div>
        ) : deals.length === 0 && columns_empty(board) ? (
          <EmptyState
            title="This pipeline has no deals yet"
            hint="A pipeline is a set of columns; a deal is a card in one of them. Create the first one and it lands in the opening column."
            action={
              <button
                type="button"
                onClick={openCreate}
                className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
              >
                New deal
              </button>
            }
          />
        ) : (
          <div className="flex gap-3 overflow-x-auto p-4" data-qa-board="pipeline">
            {board.columns.map((column, index) => {
              const cards = board.deals.filter((deal) => deal.stage_id === column.stage_id);
              return (
                <section
                  key={column.stage_id}
                  data-qa-stage={column.stage_id}
                  aria-label={`${column.name}, ${column.deal_count} deals`}
                  className={`flex w-64 shrink-0 flex-col rounded-xl border border-line bg-canvas/60 ${
                    dropStage === column.stage_id ? "border-accent ring-2 ring-accent/20" : ""
                  }`}
                  onDragOver={(event) => {
                    event.preventDefault();
                    setDropStage(column.stage_id);
                  }}
                  onDragLeave={() => setDropStage((current) => (current === column.stage_id ? null : current))}
                  onDrop={(event) => {
                    event.preventDefault();
                    setDropStage(null);
                    const id = event.dataTransfer.getData("text/plain") || dragId;
                    setDragId(null);
                    const deal = board.deals.find((entry) => entry.id === id);
                    if (deal) void requestMove(deal, column.stage_id);
                  }}
                >
                  {/* The header carries the count, the sum and the weighted sum — the three
                      numbers a person reads a column for, in that order. */}
                  <header className="border-b border-line px-3 py-2.5">
                    <div className="flex items-center justify-between gap-2">
                      <h3 className="truncate text-[12.5px] font-medium">{column.name}</h3>
                      <span className="rounded-full bg-surface px-1.5 py-0.5 text-[11px] text-muted">
                        {column.deal_count}
                      </span>
                    </div>
                    <p className="mt-0.5 text-[11.5px] text-muted">
                      {totalLabel(column.total)} · {column.probability}%
                      {column.kind === "open" ? (
                        <span className="text-muted"> → {totalLabel(column.weighted_total)}</span>
                      ) : null}
                    </p>
                  </header>

                  <div className="flex flex-1 flex-col gap-2 p-2">
                    {cards.length === 0 ? (
                      /* An empty *column* is not an empty board, and it says something different:
                          a deal exists somewhere on this pipeline, so the sentence is about this
                          stage, and the action moves it there. "No deals" in grey said nothing and
                          offered nothing, on the one screen where the most likely next act is a
                          drag. The first column offers the create form; the others offer the drop
                          target, because a card can only arrive by being dragged. */
                      <div className="flex flex-1 flex-col items-center justify-center gap-1.5 rounded-lg border border-dashed border-line px-2 py-5 text-center">
                        <p className="text-[11.5px] text-muted">
                          {column.kind === "open" ? "Nothing here yet" : `No ${column.name.toLowerCase()} yet`}
                        </p>
                        {column.kind === "open" && index === 0 ? (
                          <button
                            type="button"
                            data-qa={`empty-stage-${column.stage_id}`}
                            onClick={() => {
                              // The stage is the only field the empty column knows: the form's
                              // stage is prefilled with the column the button lives in, which is
                              // the whole point of offering it there.
                              setForm({ ...EMPTY_FORM, stage_id: column.stage_id });
                              setOutcome(null);
                            }}
                            className="inline-flex items-center gap-1 rounded-md border border-line px-2 py-1 text-[11.5px] transition hover:bg-surface"
                          >
                            <Plus className="size-3" aria-hidden /> New deal
                          </button>
                        ) : (
                          <p className="text-[11px] text-muted/80">Drag a card here</p>
                        )}
                      </div>
                    ) : null}
                    {cards.map((deal) => (
                      <article
                        key={deal.id}
                        draggable
                        data-qa-card={deal.id}
                        data-qa-stage-of={deal.stage_id}
                        tabIndex={0}
                        onDragStart={(event) => {
                          setDragId(deal.id);
                          event.dataTransfer.effectAllowed = "move";
                          event.dataTransfer.setData("text/plain", deal.id);
                        }}
                        onDragEnd={() => {
                          setDragId(null);
                          setDropStage(null);
                        }}
                        onFocus={() => setFocusedCard(deal.id)}
                        onClick={() => setFocusedCard(deal.id)}
                        onKeyDown={(event) => {
                          if (event.key === "Enter" || event.key === " ") {
                            event.preventDefault();
                            setForm({
                              ...EMPTY_FORM,
                              id: deal.id,
                              title: deal.title,
                              stage_id: deal.stage_id,
                              company_id: deal.company_id ?? "",
                              contact_id: deal.contact_id ?? "",
                              amount: deal.amount,
                              currency: deal.currency,
                              probability: deal.probability === null ? "" : String(deal.probability),
                              expected_close_on: deal.expected_close_on ?? "",
                              source: deal.source ?? "",
                            });
                          }
                          if (event.key === "Delete" || event.key === "Backspace") {
                            event.preventDefault();
                            void archiveCrmDeal(deal.id)
                              .then(() => {
                                setNotice(`${deal.title} archived.`);
                                setReloadToken((token) => token + 1);
                              })
                              .catch((problem) =>
                                setError(
                                  problem instanceof ApiError ? problem.message : "The deal could not be archived.",
                                ),
                              );
                          }
                        }}
                        aria-label={`${deal.title}, ${deal.currency} ${deal.amount}, in ${deal.stage_name}`}
                        className={`cursor-grab rounded-lg border bg-surface p-2.5 text-left transition hover:border-accent/40 ${
                          focusedCard === deal.id ? "border-accent ring-2 ring-accent/20" : "border-line"
                        }`}
                      >
                        <p className="text-[12.5px] font-medium leading-snug">{deal.title}</p>
                        {deal.company_name ? (
                          <p className="mt-0.5 truncate text-[11.5px] text-muted">{deal.company_name}</p>
                        ) : null}
                        <div className="mt-1.5 flex items-center justify-between gap-2">
                          <span className="text-[12px] font-medium tabular-nums">
                            {money(deal.amount, deal.currency)}
                          </span>
                          {deal.probability === null ? null : (
                            <CrmTag value={`${deal.probability}%`} />
                          )}
                        </div>
                        <p className="mt-1.5 flex items-center gap-1.5 text-[11px] text-muted">
                          {/* Stale is the one thing a pipeline can tell you that a spreadsheet of
                              opportunities cannot: that nothing has happened for a month. */}
                          {deal.stale ? (
                            <span className="inline-flex items-center gap-1 text-warning">
                              <AlertTriangle className="size-3" aria-hidden />
                              {age(deal.days_in_stage)}
                            </span>
                          ) : (
                            <span>{age(deal.days_in_stage)}</span>
                          )}
                          {deal.expected_close_on ? <span>· closes {deal.expected_close_on}</span> : null}
                        </p>
                        {deal.owner_name ? (
                          <p className="mt-1.5 flex items-center gap-1.5 text-[11px] text-muted">
                            <CrmAvatar initials={deal.owner_name.slice(0, 2).toUpperCase()} tone="quiet" />
                            {deal.owner_name}
                          </p>
                        ) : null}
                        {deal.lost_reason ? (
                          <p className="mt-1.5 text-[11px] text-muted">Reason: {deal.lost_reason}</p>
                        ) : null}
                        {/* The copilot lives *on* the card, not in a row menu: on a board the card
                            is the record, and a menu would make every question start with "which
                            card?". Focus opens the panel for this deal and nothing else. */}
                        <button
                          type="button"
                          data-qa="deal-copilot"
                          data-copilot-deal={deal.id}
                          onClick={(event) => {
                            event.stopPropagation();
                            setFocusedCard(deal.id);
                            setCopilotAnswer(null);
                            setCopilotError(null);
                            setCopilotOpen(true);
                          }}
                          onKeyDown={(event) => {
                            if (event.key === "Enter" || event.key === " ") event.stopPropagation();
                          }}
                          aria-label={`Ask the copilot about ${deal.title}`}
                          className="mt-2 inline-flex items-center gap-1 rounded-md border border-line px-1.5 py-0.5 text-[11px] text-muted transition hover:border-accent/40 hover:text-ink"
                        >
                          <Sparkles className="size-3" aria-hidden />
                          Copilot
                        </button>
                      </article>
                    ))}
                  </div>
                </section>
              );
            })}
          </div>
        )
      ) : list === null ? (
        <div className="divide-y divide-line" aria-busy="true">
          {[0, 1, 2, 3, 4].map((row) => (
            <div key={row} className="h-12 animate-pulse bg-canvas" />
          ))}
        </div>
      ) : list.length === 0 ? (
        <EmptyState
          title={search ? `No deal matches “${search}”` : "No deals yet"}
          hint={
            search
              ? "Clear the search to see every deal on this pipeline."
              : "Create the first deal and it lands in the pipeline's opening column."
          }
          action={
            <button
              type="button"
              onClick={openCreate}
              className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
            >
              New deal
            </button>
          }
        />
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full min-w-[720px] border-collapse text-left">
            <thead className="sticky top-0 bg-surface">
              <tr className="border-b border-line text-[11.5px] uppercase tracking-wide text-muted">
                <th scope="col" className="px-4 py-2.5 font-medium">Deal</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Company</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Stage</th>
                <th scope="col" className="px-4 py-2.5 text-right font-medium">Value</th>
                <th scope="col" className="px-4 py-2.5 text-right font-medium">Probability</th>
                <th scope="col" className="px-4 py-2.5 font-medium">Closes</th>
                <th scope="col" className="px-4 py-2.5 font-medium">In stage</th>
              </tr>
            </thead>
            <tbody>
              {list.map((deal) => (
                <tr
                  key={deal.id}
                  data-qa-row={deal.id}
                  onClick={() => setForm({
                    ...EMPTY_FORM,
                    id: deal.id,
                    title: deal.title,
                    stage_id: deal.stage_id,
                    company_id: deal.company_id ?? "",
                    contact_id: deal.contact_id ?? "",
                    amount: deal.amount,
                    currency: deal.currency,
                    probability: deal.probability === null ? "" : String(deal.probability),
                    expected_close_on: deal.expected_close_on ?? "",
                    source: deal.source ?? "",
                  })}
                  className="cursor-pointer border-b border-line text-[12.5px] transition odd:bg-canvas/40 hover:bg-canvas"
                >
                  <td className="px-4 py-2.5 font-medium">{deal.title}</td>
                  <td className="px-4 py-2.5 text-muted">{deal.company_name ?? "—"}</td>
                  <td className="px-4 py-2.5 text-muted">{deal.stage_name}</td>
                  <td className="px-4 py-2.5 text-right tabular-nums">{money(deal.amount, deal.currency)}</td>
                  <td className="px-4 py-2.5 text-right tabular-nums text-muted">
                    {deal.probability === null ? "—" : `${deal.probability}%`}
                  </td>
                  <td className="px-4 py-2.5 text-muted">{deal.expected_close_on ?? "—"}</td>
                  <td className="px-4 py-2.5 text-muted">
                    <span className={deal.stale ? "text-warning" : undefined}>{age(deal.days_in_stage)}</span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}

      {mode === "board" && board !== null && deals.length > 0 ? (
        <div className="flex flex-wrap items-center gap-x-6 gap-y-1 border-t border-line px-4 py-2.5 text-[11.5px] text-muted">
          <span>
            Open value <strong className="font-medium text-ink">{totalLabel(openTotal)}</strong>
          </span>
          <span>
            Weighted forecast{" "}
            <strong className="font-medium text-ink">{totalLabel(forecast)}</strong>
          </span>
          <span className="inline-flex items-center gap-1">
            <ArrowLeftRight className="size-3" aria-hidden />
            Drag a card, or focus one and press ctrl + ←/→
          </span>
        </div>
      ) : null}

      {form ? (
        <div className="fixed inset-0 z-30 flex items-start justify-center overflow-y-auto bg-ink/20 px-4 py-10">
          <form
            id="crm-deal-form"
            onSubmit={(event) => {
              event.preventDefault();
              void saveForm();
            }}
            className="w-full max-w-lg rounded-xl border border-line bg-surface p-5 shadow-xl"
          >
            <div className="flex items-center justify-between">
              <h3 className="text-[14px] font-medium">{form.id ? "Edit deal" : "New deal"}</h3>
              <button
                type="button"
                onClick={() => setForm(null)}
                aria-label="Close"
                className="rounded p-1 text-muted hover:bg-canvas hover:text-ink"
              >
                <X className="size-4" aria-hidden />
              </button>
            </div>

            <div className="mt-4 grid gap-3">
              <label className="grid gap-1 text-[12px]">
                <span className="font-medium">Title</span>
                <input
                  id="crm-deal-title"
                  ref={titleRef}
                  value={form.title}
                  onChange={(event) => setForm({ ...form, title: event.target.value })}
                  autoFocus
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
                />
                {fieldError?.field === "title" ? (
                  <span id="crm-deal-title-error" className="text-[11.5px] text-danger">
                    {fieldError.message}
                  </span>
                ) : null}
              </label>

              <div className="grid gap-3 sm:grid-cols-2">
                <label className="grid gap-1 text-[12px]">
                  <span className="font-medium">Value</span>
                  <input
                    id="crm-deal-amount"
                    value={form.amount}
                    onChange={(event) => setForm({ ...form, amount: event.target.value })}
                    placeholder="12500"
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
                  />
                  {fieldError?.field === "amount" ? (
                    <span id="crm-deal-amount-error" className="text-[11.5px] text-danger">
                      {fieldError.message}
                    </span>
                  ) : null}
                </label>

                <label className="grid gap-1 text-[12px]">
                  <span className="font-medium">Currency</span>
                  <select
                    id="crm-deal-currency"
                    value={form.currency}
                    onChange={(event) => setForm({ ...form, currency: event.target.value })}
                    className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
                  >
                    {CRM_CURRENCIES.map((code) => (
                      <option key={code} value={code}>
                        {code}
                      </option>
                    ))}
                  </select>
                  {fieldError?.field === "currency" ? (
                    <span className="text-[11.5px] text-danger">{fieldError.message}</span>
                  ) : null}
                </label>

                <label className="grid gap-1 text-[12px]">
                  <span className="font-medium">Probability (%)</span>
                  <input
                    id="crm-deal-probability"
                    value={form.probability}
                    onChange={(event) => setForm({ ...form, probability: event.target.value })}
                    placeholder="the stage's own"
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
                  />
                  {fieldError?.field === "probability" ? (
                    <span className="text-[11.5px] text-danger">{fieldError.message}</span>
                  ) : null}
                </label>

                <label className="grid gap-1 text-[12px]">
                  <span className="font-medium">Expected close</span>
                  <input
                    id="crm-deal-close"
                    type="date"
                    value={form.expected_close_on}
                    onChange={(event) => setForm({ ...form, expected_close_on: event.target.value })}
                    className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
                  />
                </label>
              </div>

              <label className="grid gap-1 text-[12px]">
                <span className="font-medium">Stage</span>
                <select
                  id="crm-deal-stage"
                  value={form.stage_id}
                  onChange={(event) => setForm({ ...form, stage_id: event.target.value })}
                  className="rounded-lg border border-line bg-surface px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
                >
                  {stages.map((stage) => (
                    <option key={stage.id} value={stage.id}>
                      {stage.name} ({stage.kind})
                    </option>
                  ))}
                </select>
                {fieldError?.field === "stage_id" ? (
                  <span className="text-[11.5px] text-danger">{fieldError.message}</span>
                ) : null}
              </label>

              <label className="grid gap-1 text-[12px]">
                <span className="font-medium">Source</span>
                <input
                  id="crm-deal-source"
                  value={form.source}
                  onChange={(event) => setForm({ ...form, source: event.target.value })}
                  placeholder="Inbound, referral, fair…"
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
                />
              </label>
            </div>

            <div className="mt-5 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setForm(null)}
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
              >
                Cancel
              </button>
              <button
                type="submit"
                disabled={saving}
                className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
              >
                {saving ? "Saving…" : form.id ? "Save" : "Create deal"}
              </button>
            </div>
          </form>
        </div>
      ) : null}

      {outcome ? (
        <div className="fixed inset-0 z-30 flex items-center justify-center bg-ink/20 px-4">
          <div
            role="dialog"
            aria-modal="true"
            aria-labelledby="crm-outcome-title"
            className="w-full max-w-md rounded-xl border border-line bg-surface p-5 shadow-xl"
          >
            <h3 id="crm-outcome-title" className="text-[14px] font-medium">
              Mark “{outcome.deal.title}” as {outcome.kind}
            </h3>
            {outcome.kind === "lost" ? (
              <label className="mt-3 grid gap-1 text-[12px]">
                <span className="font-medium">Why was it lost?</span>
                <input
                  id="crm-lost-reason"
                  value={outcome.reason}
                  onChange={(event) => setOutcome({ ...outcome, reason: event.target.value })}
                  placeholder="chose a competitor, budget frozen…"
                  autoFocus
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
                />
                <span className="text-[11px] text-muted">
                  A lost deal has to say why — without it the loss report cannot tell a lost
                  price from a lost deal.
                </span>
              </label>
            ) : (
              <label className="mt-3 grid gap-1 text-[12px]">
                <span className="font-medium">Close date</span>
                <input
                  id="crm-won-close"
                  type="date"
                  value={outcome.close_on}
                  onChange={(event) => setOutcome({ ...outcome, close_on: event.target.value })}
                  className="rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
                />
                <span className="text-[11px] text-muted">
                  The day the deal was won, which is what the “won this quarter” figure counts.
                </span>
              </label>
            )}
            <div className="mt-5 flex justify-end gap-2">
              <button
                type="button"
                onClick={() => setOutcome(null)}
                className="rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
              >
                Cancel
              </button>
              <button
                type="button"
                id="crm-outcome-confirm"
                onClick={() => void confirmOutcome()}
                disabled={outcome.kind === "lost" && outcome.reason.trim().length === 0}
                className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
              >
                Mark {outcome.kind}
              </button>
            </div>
          </div>
        </div>
      ) : null}

      {copilotOpen && copilotDeal ? (
        <aside
          role="dialog"
          aria-modal="false"
          aria-labelledby="crm-copilot-title"
          data-qa="copilot-panel"
          data-copilot-deal={copilotDeal.id}
          className="fixed inset-y-0 right-0 z-30 flex w-full max-w-sm flex-col border-l border-line bg-surface shadow-2xl"
        >
          <div className="flex items-start justify-between gap-2 border-b border-line px-4 py-3">
            <div className="min-w-0">
              <h3 id="crm-copilot-title" className="text-[13.5px] font-medium">
                Copilot
              </h3>
              <p className="truncate text-[11.5px] text-muted">{copilotDeal.title}</p>
            </div>
            <button
              type="button"
              ref={copilotClose}
              data-qa="copilot-close"
              onClick={() => setCopilotOpen(false)}
              aria-label="Close the copilot"
              className="rounded p-1 text-muted transition hover:bg-canvas hover:text-ink"
            >
              <X className="size-4" aria-hidden />
            </button>
          </div>

          <div className="flex gap-2 border-b border-line px-4 py-3">
            <button
              type="button"
              id="crm-copilot-summarize"
              onClick={() => void runCopilot(copilotDeal, "summarize")}
              disabled={copilotBusy !== null}
              className="flex-1 rounded-lg bg-accent px-2.5 py-1.5 text-[12px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
            >
              {copilotBusy === "summarize" ? "Summarizing…" : "Summarize deal"}
            </button>
            <button
              type="button"
              id="crm-copilot-follow-up"
              onClick={() => void runCopilot(copilotDeal, "follow-up")}
              disabled={copilotBusy !== null}
              className="flex-1 rounded-lg border border-line px-2.5 py-1.5 text-[12px] transition hover:bg-canvas disabled:opacity-50"
            >
              {copilotBusy === "follow-up" ? "Drafting…" : "Draft follow-up"}
            </button>
          </div>

          <div className="flex-1 overflow-y-auto px-4 py-3">
            {copilotError ? (
              <p
                role="alert"
                data-qa="copilot-error"
                className="rounded-lg border border-danger/30 bg-danger/5 px-3 py-2.5 text-[12px] text-danger"
              >
                {copilotError}
              </p>
            ) : copilotAnswer ? (
              <figure data-qa="copilot-answer" data-copilot-action={copilotAnswer.action}>
                {/* The draft marker is the feature's promise, so it is a *fact* here and not a
                    nicety: the answer came back `is_draft: false` and the panel says so. */}
                <figcaption className="mb-2 flex flex-wrap items-center gap-x-2 gap-y-1 text-[11px] text-muted">
                  <span className="font-medium text-ink">
                    {copilotAnswer.action === "summarize" ? "Summary" : "Follow-up draft"}
                  </span>
                  <span>· {copilotAnswer.model}</span>
                  <span
                    data-qa="copilot-draft-flag"
                    className={`rounded-full px-1.5 py-0.5 ${
                      copilotAnswer.is_draft
                        ? "bg-surface text-muted"
                        : "border border-warning/40 text-warning"
                    }`}
                  >
                    {copilotAnswer.is_draft ? "draft" : "written to the record"}
                  </span>
                </figcaption>
                {/* A text node, on purpose: the server's sanitiser already reduced the answer to
                    plain text, and rendering it as markup here would put that promise back in
                    the hands of a later refactor. */}
                <p className="whitespace-pre-wrap text-[12.5px] leading-relaxed">{copilotAnswer.draft}</p>
              </figure>
            ) : (
              <p data-qa="copilot-empty" className="text-[12px] text-muted">
                Nothing asked yet. Both answers are read-only suggestions — nothing is written to
                the deal without an explicit save, and every call is audited with the deal, the
                model and the size of the answer.
              </p>
            )}
          </div>
        </aside>
      ) : null}
    </CrmShell>
  );
}

/** `true` when the board has no cards at all — a *filtered* list showing nothing is a different
 *  thing, and the two deserve different empty states. */
function columns_empty(board: CrmBoard): boolean {
  return board.deals.length === 0;
}

/** Put a card in another column of a board, without touching anything else. */
function relocate(board: CrmBoard, dealId: string, stageId: string): CrmBoard {
  const column = board.columns.find((entry) => entry.stage_id === stageId);
  return {
    ...board,
    deals: board.deals.map((deal) =>
      deal.id === dealId
        ? {
            ...deal,
            stage_id: stageId,
            stage_name: column?.name ?? deal.stage_name,
            stage_kind: column?.kind ?? deal.stage_kind,
          }
        : deal,
    ),
  };
}

/** The pipeline editor: the stages, their kinds, and a drag to reorder them. */
export function PipelinesSettingsView() {
  const [pipelines, setPipelines] = useState<CrmPipeline[] | null>(null);
  const [selected, setSelected] = useState<string>("");
  const [rows, setRows] = useState<CrmPipeline["stages"]>([]);
  const [error, setError] = useState<ScreenErrorValue>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [reloadToken, setReloadToken] = useState(0);

  // The editor edits a *tenant's* stages, so it needs the same tenant the board drew.
  const { organizationId } = useCrmTenant();

  useEffect(() => {
    fetchCrmPipelines(organizationId ?? undefined)
      .then((rows_) => {
        setPipelines(rows_);
        const first = rows_[0]?.id ?? "";
        setSelected((current) => current || first);
      })
      .catch((problem) =>
        setError(toScreenError(problem, "The pipelines could not be loaded.")),
      );
  }, [reloadToken, organizationId]);

  useEffect(() => {
    const pipeline = pipelines?.find((entry) => entry.id === selected);
    setRows(pipeline ? pipeline.stages : []);
  }, [pipelines, selected]);

  const move = useCallback((index: number, delta: number) => {
    setRows((current) => {
      const next = [...current];
      const target = index + delta;
      if (target < 0 || target >= next.length) return current;
      const [row] = next.splice(index, 1);
      next.splice(target, 0, row);
      return next;
    });
  }, []);

  const save = useCallback(async () => {
    setSaving(true);
    setError(null);
    try {
      const { saveCrmPipelineStages } = await import("@/lib/crm");
      await saveCrmPipelineStages(
        selected,
        rows.map((stage) => ({
          name: stage.name,
          kind: stage.kind,
          probability: stage.probability,
        })),
      );
      setNotice("The stages were saved.");
      setReloadToken((token) => token + 1);
    } catch (problem) {
      setError(toScreenError(problem, "The stages could not be saved."));
    } finally {
      setSaving(false);
    }
  }, [selected, rows]);

  return (
    <CrmShell
      title="Pipeline settings"
      description="The columns a deal moves through, their order, and which one means won and which means lost"
      entity="deals"
      availableColumns={["title"]}
      statuses={["open", "won", "lost"]}
      total={rows.length}
      nextCursor={null}
      loadingMore={false}
      loadMore={() => {}}
      onCreate={() =>
        setRows((current) => [
          ...current,
          {
            id: `new-${current.length}`,
            organization_id: "",
            pipeline_id: selected,
            name: `Stage ${current.length + 1}`,
            kind: "open",
            position: current.length,
            probability: 0,
          },
        ])
      }
      rowIds={rows.map((stage) => stage.id)}
      keyboard={{ onEdit: () => {}, onOpen: () => {} }}
      toolbarExtra={
        <button
          type="button"
          id="crm-stages-save"
          onClick={() => void save()}
          disabled={saving || !selected}
          className="rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white transition hover:bg-accent-strong disabled:opacity-50"
        >
          {saving ? "Saving…" : "Save stages"}
        </button>
      }
    >
      {error ? (
        <ErrorStrip
          error={error}
          onRetry={() => setReloadToken((token) => token + 1)}
          qa="crm-stages-error"
        />
      ) : null}
      {notice ? (
        <p className="border-b border-line bg-canvas px-4 py-2.5 text-[12px] text-muted">{notice}</p>
      ) : null}

      <div className="flex items-center gap-2 border-b border-line px-4 py-2.5">
        <label className="flex items-center gap-1.5">
          <span className="text-[12px] text-muted">Pipeline</span>
          <select
            id="crm-settings-pipeline"
            value={selected}
            onChange={(event) => setSelected(event.target.value)}
            className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12px] outline-none focus:border-accent"
          >
            {(pipelines ?? []).map((pipeline) => (
              <option key={pipeline.id} value={pipeline.id}>
                {pipeline.name}
              </option>
            ))}
          </select>
        </label>
      </div>

      {rows.length === 0 ? (
        <EmptyState
          title="This pipeline has no stages"
          hint="A board is a list of columns. Add the first one and a deal will land in it."
          action={
            <button
              type="button"
              onClick={() =>
                setRows([
                  {
                    id: "new-0",
                    organization_id: "",
                    pipeline_id: selected,
                    name: "New",
                    kind: "open",
                    position: 0,
                    probability: 10,
                  },
                ])
              }
              className="inline-flex items-center gap-1.5 rounded-lg bg-accent px-3 py-1.5 text-[12.5px] font-medium text-white"
            >
              <Plus className="size-3.5" aria-hidden /> Add a stage
            </button>
          }
        />
      ) : (
        <ul className="divide-y divide-line">
          {rows.map((stage, index) => (
            <li
              key={stage.id}
              data-qa-stage-row={stage.id}
              className="flex flex-wrap items-end gap-2 px-4 py-3"
            >
              <label className="grid gap-1 text-[12px]">
                <span className="text-[11px] text-muted">Name</span>
                <input
                  id={`crm-stage-name-${index}`}
                  value={stage.name}
                  onChange={(event) =>
                    setRows((current) =>
                      current.map((entry, at) =>
                        at === index ? { ...entry, name: event.target.value } : entry,
                      ),
                    )
                  }
                  className="w-44 rounded-lg border border-line bg-canvas px-2.5 py-1.5 text-[12.5px] outline-none focus:border-accent"
                />
              </label>

              <label className="grid gap-1 text-[12px]">
                <span className="text-[11px] text-muted">Kind</span>
                <select
                  id={`crm-stage-kind-${index}`}
                  value={stage.kind}
                  onChange={(event) =>
                    setRows((current) =>
                      current.map((entry, at) =>
                        at === index
                          ? { ...entry, kind: event.target.value as "open" | "won" | "lost" }
                          : entry,
                      ),
                    )
                  }
                  className="rounded-lg border border-line bg-surface px-2 py-1.5 text-[12.5px] outline-none focus:border-accent"
                >
                  <option value="open">Open</option>
                  <option value="won">Won</option>
                  <option value="lost">Lost</option>
                </select>
              </label>

              <label className="grid gap-1 text-[12px]">
                <span className="text-[11px] text-muted">Probability</span>
                <input
                  id={`crm-stage-probability-${index}`}
                  value={stage.probability}
                  onChange={(event) =>
                    setRows((current) =>
                      current.map((entry, at) =>
                        at === index ? { ...entry, probability: Number(event.target.value) } : entry,
                      ),
                    )
                  }
                  className="w-20 rounded-lg border border-line bg-canvas px-2 py-1.5 text-[12.5px] outline-none focus:border-accent"
                />
              </label>

              <div className="ml-auto flex items-center gap-1">
                <button
                  type="button"
                  aria-label={`Move ${stage.name} earlier`}
                  disabled={index === 0}
                  onClick={() => move(index, -1)}
                  className="rounded-lg border border-line px-2 py-1.5 text-[12px] transition hover:bg-canvas disabled:opacity-40"
                >
                  ↑
                </button>
                <button
                  type="button"
                  aria-label={`Move ${stage.name} later`}
                  disabled={index === rows.length - 1}
                  onClick={() => move(index, 1)}
                  className="rounded-lg border border-line px-2 py-1.5 text-[12px] transition hover:bg-canvas disabled:opacity-40"
                >
                  ↓
                </button>
                <button
                  type="button"
                  aria-label={`Remove ${stage.name}`}
                  onClick={() => setRows((current) => current.filter((_, at) => at !== index))}
                  className="rounded-lg border border-line px-2 py-1.5 text-[12px] transition hover:bg-canvas"
                >
                  <X className="size-3.5" aria-hidden />
                </button>
              </div>
            </li>
          ))}
          <li className="px-4 py-3">
            <button
              type="button"
              id="crm-stage-add"
              onClick={() =>
                setRows((current) => [
                  ...current,
                  {
                    id: `new-${current.length}`,
                    organization_id: "",
                    pipeline_id: selected,
                    name: `Stage ${current.length + 1}`,
                    kind: "open",
                    position: current.length,
                    probability: 0,
                  },
                ])
              }
              className="inline-flex items-center gap-1.5 rounded-lg border border-line px-3 py-1.5 text-[12.5px] transition hover:bg-canvas"
            >
              <Plus className="size-3.5" aria-hidden /> Add a stage
            </button>
          </li>
        </ul>
      )}
    </CrmShell>
  );
}

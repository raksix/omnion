"use client";

/**
 * The stock badge and the shared pieces of the inventory screens (REQ-053, slice 2).
 *
 * ## The one rule in this file
 *
 * **A status is never carried by colour alone.** Every badge prints a sentence next to its
 * colour, and the sentence is generated from the module's own vocabulary
 * (`statusLabel`), never re-derived in the component. That is not a style preference: the
 * criterion says "badges with text labels (not colour alone)", and the deeper reason is that
 * eight percent of the men in a warehouse cannot read a red dot, so a red-only badge is a badge
 * that tells a third of the room the shelf is fine.
 *
 * ## What the drawer previews, and why it asks the server
 *
 * The adjust drawer calls `POST /movements/preview` on every keystroke rather than computing the
 * result in the browser. A second implementation of the module's arithmetic in TypeScript is a
 * second answer: it will disagree about a `counted` mode, about the negative-stock rule, and
 * about the reorder-point crossing, and it will disagree *silently*, because a preview that is
 * off by one is a perfectly plausible-looking number. The drawer shows the server's sentence.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, Loader2, ScanLine, X } from "lucide-react";

import {
  formatQuantity,
  movementTone,
  previewMovement,
  recordMovement,
  signedText,
  statusLabel,
  statusTone,
  type Location,
  type Movement,
  type MovementPreview,
  type Quantity,
  type RecordOutcome,
  type StockLevel,
  type StockStatus,
} from "@/lib/inventory";

/** A status badge: colour plus the sentence that carries the meaning. */
export function StockStatusBadge({ status }: { status: StockStatus }) {
  return (
    <span
      data-qa-inventory-status={status}
      className={`inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-[11.5px] font-medium ${statusTone(status)}`}
    >
      {status === "negative" || status === "below_reorder" ? (
        <AlertTriangle aria-hidden className="h-3 w-3" />
      ) : null}
      {statusLabel(status)}
    </span>
  );
}

/** A movement's signed quantity — the sign is in the text, the colour only repeats it. */
export function SignedQuantity({ movement }: { movement: Movement }) {
  return (
    <span data-qa-inventory-signed className={`font-mono tabular-nums ${movementTone(movement)}`}>
      {signedText(movement)}
    </span>
  );
}

/** A quantity in the module's own three decimals, right-aligned so the column scans. */
export function QuantityCell({ value }: { value: Quantity | null | undefined }) {
  return (
    <span data-qa-inventory-quantity className="font-mono tabular-nums">
      {formatQuantity(value)}
    </span>
  );
}

/** A relative "3 days ago" with the absolute timestamp in the tooltip. */
export function RelativeTime({ at }: { at: string | null }) {
  if (!at) {
    return <span className="text-muted">Never</span>;
  }
  const then = new Date(at).getTime();
  const minutes = Math.round((Date.now() - then) / 60_000);
  const text =
    minutes < 1
      ? "just now"
      : minutes < 60
        ? `${minutes} min ago`
        : minutes < 60 * 24
          ? `${Math.round(minutes / 60)} h ago`
          : `${Math.round(minutes / (60 * 24))} d ago`;
  return (
    <time dateTime={at} title={new Date(at).toLocaleString()} className="text-muted">
      {text}
    </time>
  );
}

// ---------------------------------------------------------------------------------------------
// The adjust drawer
// ---------------------------------------------------------------------------------------------

/** What the drawer starts from. */
export type AdjustTarget =
  | { kind: "item"; itemId: string; sku: string; name: string; locationId: string }
  | { kind: "stock"; itemId: string; sku: string; name: string; locationId: string; onHand: Quantity };

/** What the drawer reports back. */
export type AdjustResult =
  | { outcome: "recorded"; movement: Movement }
  | { outcome: "awaiting_approval"; amount: Quantity; threshold: Quantity }
  | { outcome: "cancelled" };

/**
 * The adjust drawer, used from the item list, the stock list and the ledger.
 *
 * Four decisions, each a way this could have lied:
 *
 * * **The resulting quantity comes from the server, before the save.** The drawer asks
 *   `POST /movements/preview` on every keystroke and prints what the module's own arithmetic
 *   says would happen. A browser-side calculation would be a second implementation.
 * * **The preview answers even when the write would be refused**, because the refusal *is* the
 *   preview: "this would leave -3.000, and 6.000 are available" is the sentence the operator
 *   needs, and it is more use than a red border.
 * * **The save does not decide whether an approval is needed.** It posts, and the server answers
 *   `recorded` or `awaiting_approval`; the drawer prints the second as "waiting on a decision"
 *   rather than as a success, because the stock has not moved.
 * * **A refused save keeps what was typed.** The alternative loses the number somebody just
 *   counted off a shelf, and teaches them to count twice.
 */
export function AdjustDrawer({
  target,
  locations,
  onClose,
  onDone,
}: {
  target: AdjustTarget;
  locations: Location[];
  onClose: () => void;
  onDone: (result: AdjustResult) => void;
}) {
  const [locationId, setLocationId] = useState(target.locationId);
  const [mode, setMode] = useState<"delta" | "counted">("delta");
  const [quantity, setQuantity] = useState("");
  const [reason, setReason] = useState("correction");
  const [note, setNote] = useState("");
  const [preview, setPreview] = useState<MovementPreview | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const quantityRef = useRef<HTMLInputElement>(null);
  // The last preview wins: two in-flight previews can resolve out of order, and a preview that
  // answers about an earlier keystroke is worse than no preview at all.
  const previewSeq = useRef(0);

  useEffect(() => {
    quantityRef.current?.focus();
  }, []);

  // Escape closes, and the panel itself is the scroll container so Tab reaches the number pad
  // first on a phone without the page behind it scrolling.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const runPreview = useCallback(async () => {
    if (!quantity.trim()) {
      setPreview(null);
      setPreviewError(null);
      return;
    }
    const seq = previewSeq.current + 1;
    previewSeq.current = seq;
    try {
      const answer = await previewMovement({
        item_id: target.itemId,
        location_id: locationId,
        quantity: quantity.trim(),
        mode,
        reason,
      });
      if (previewSeq.current === seq) {
        setPreview(answer);
        setPreviewError(null);
      }
    } catch (caught) {
      if (previewSeq.current === seq) {
        setPreview(null);
        // The refusal sentence, verbatim. A drawer that says "invalid" where the server said
        // "this would leave -3.000 (available 6.000)" has thrown away the only useful part.
        setPreviewError(caught instanceof Error ? caught.message : String(caught));
      }
    }
  }, [target.itemId, locationId, quantity, mode, reason]);

  useEffect(() => {
    // Debounced: a scanner pastes a code and a number in one burst, and one request per
    // character is one request per character.
    const handle = setTimeout(() => {
      void runPreview();
    }, 180);
    return () => clearTimeout(handle);
  }, [runPreview]);

  const active = useMemo(
    () => locations.filter((location) => location.active) || locations,
    [locations],
  );

  async function save() {
    if (!quantity.trim()) {
      setSaveError("Type the quantity first.");
      return;
    }
    setSaving(true);
    setSaveError(null);
    try {
      const answer: RecordOutcome = await recordMovement({
        item_id: target.itemId,
        location_id: locationId,
        quantity: quantity.trim(),
        mode,
        reason,
        note: note.trim() || undefined,
      });
      if (answer.status === "recorded") {
        onDone({ outcome: "recorded", movement: answer.movement });
      } else {
        onDone({
          outcome: "awaiting_approval",
          amount: answer.approval.amount,
          threshold: answer.approval.threshold,
        });
      }
    } catch (caught) {
      setSaveError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div
      data-qa-inventory-drawer
      className="fixed inset-0 z-50 flex items-end justify-center bg-black/30 p-0 sm:items-center sm:p-4"
      onClick={(event) => {
        if (event.target === event.currentTarget) {
          onClose();
        }
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label={`Adjust ${target.name}`}
        // A full-screen sheet on a phone and a centred dialog on a desk: the criterion asks for
        // one-handed use, and a 480px dialog floating in the middle of a 390px screen is not it.
        className="flex max-h-[92vh] w-full flex-col overflow-hidden rounded-t-2xl border bg-white shadow-xl sm:max-w-lg sm:rounded-2xl"
      >
        <header className="flex items-start justify-between gap-3 border-b px-4 py-3">
          <div>
            <h2 className="text-sm font-semibold">Adjust stock</h2>
            <p className="text-[12px] text-muted">
              {target.sku} · {target.name}
            </p>
          </div>
          <button
            type="button"
            onClick={onClose}
            aria-label="Close"
            className="rounded p-1 text-muted hover:bg-stone-100"
          >
            <X aria-hidden className="h-4 w-4" />
          </button>
        </header>

        <div className="flex-1 overflow-y-auto px-4 py-3">
          <label className="block text-[12px] font-medium">
            Location
            <select
              value={locationId}
              onChange={(event) => setLocationId(event.target.value)}
              className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
            >
              {active.map((location) => (
                <option key={location.id} value={location.id}>
                  {location.warehouse_code} / {location.code} — {location.name}
                </option>
              ))}
            </select>
          </label>

          <fieldset className="mt-3">
            <legend className="text-[12px] font-medium">What you are entering</legend>
            <div className="mt-1 flex gap-4">
              {(["delta", "counted"] as const).map((option) => (
                <label key={option} className="flex items-center gap-1.5 text-[13px]">
                  <input
                    type="radio"
                    name="mode"
                    value={option}
                    checked={mode === option}
                    onChange={() => setMode(option)}
                  />
                  {option === "delta" ? "Change by" : "Counted total"}
                </label>
              ))}
            </div>
            <p className="mt-1 text-[11.5px] text-muted">
              {mode === "delta"
                ? "A positive number adds, a negative one removes."
                : "The number you counted on the shelf. The change is worked out from what the system believes."}
            </p>
          </fieldset>

          <label className="mt-3 block text-[12px] font-medium">
            {mode === "delta" ? "Change by" : "Counted total"}
            <input
              ref={quantityRef}
              data-qa-inventory-drawer-quantity
              // `inputMode` is what puts the number pad on a phone; `type="number"` does not,
              // and a warehouse worker is not going to find a minus key on a desktop keyboard.
              inputMode="decimal"
              value={quantity}
              onChange={(event) => setQuantity(event.target.value)}
              placeholder={mode === "delta" ? "-4 or 12" : "6"}
              className="mt-1 w-full rounded border px-2 py-2 font-mono text-[15px] tabular-nums"
            />
          </label>

          <label className="mt-3 block text-[12px] font-medium">
            Reason
            <select
              value={reason}
              onChange={(event) => setReason(event.target.value)}
              className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
            >
              {REASONS.map((option) => (
                <option key={option.value} value={option.value}>
                  {option.label}
                </option>
              ))}
            </select>
          </label>

          <label className="mt-3 block text-[12px] font-medium">
            Note <span className="font-normal text-muted">(optional)</span>
            <input
              value={note}
              onChange={(event) => setNote(event.target.value)}
              maxLength={500}
              className="mt-1 w-full rounded border px-2 py-1.5 text-[13px]"
            />
          </label>

          {/* The preview, and the refusal, in the same place: the operator reads one box.

              The preview's three figures are one column on a phone (`sm:grid-cols-3` from 640px):
              "On hand now / After this / Status" is a comparison a person reads across, and at
              390px three cells of ~110px leave the figures competing for the same line.

              The comment sits out here rather than inside the `preview ? (…)` branch below, because
              a ternary arm has to be a single expression and a comment is not one — and it must not
              quote a brace-comment token verbatim either, because that closes *this* comment at the
              first `*` slash and the rest of the paragraph parses as JSX text. Typecheck caught
              both on the way in, which is the argument for running it before the commit. */}
          <div className="mt-4 rounded border bg-stone-50 px-3 py-2" data-qa-inventory-drawer-preview>
            {previewError ? (
              <p className="text-[12.5px] text-red-700" data-qa-inventory-drawer-refusal>
                {previewError}
              </p>
            ) : preview ? (
              <dl className="grid gap-2 text-[12px] sm:grid-cols-3">
                <div>
                  <dt className="text-muted">On hand now</dt>
                  <dd className="font-mono tabular-nums">{preview.on_hand_before}</dd>
                </div>
                <div>
                  <dt className="text-muted">After this</dt>
                  <dd
                    data-qa-inventory-drawer-after
                    className="font-mono tabular-nums font-semibold"
                  >
                    {preview.on_hand_after}
                  </dd>
                </div>
                <div>
                  <dt className="text-muted">Status</dt>
                  <dd>
                    <StockStatusBadge status={preview.status} />
                  </dd>
                </div>
              </dl>
            ) : (
              <p className="text-[12px] text-muted">
                The resulting quantity appears here before you save.
              </p>
            )}
          </div>

          {saveError ? (
            <p
              data-qa-inventory-drawer-error
              className="mt-3 rounded border border-red-200 bg-red-50 px-3 py-2 text-[12.5px] text-red-800"
            >
              {saveError}
            </p>
          ) : null}
        </div>

        <footer className="flex items-center justify-end gap-2 border-t px-4 py-3">
          <button
            type="button"
            onClick={onClose}
            className="rounded border px-3 py-1.5 text-[13px]"
          >
            Cancel
          </button>
          <button
            type="button"
            data-qa-inventory-drawer-save
            onClick={() => void save()}
            disabled={saving || !quantity.trim()}
            className="inline-flex items-center gap-1.5 rounded bg-stone-900 px-3 py-1.5 text-[13px] font-medium text-white disabled:opacity-50"
          >
            {saving ? <Loader2 aria-hidden className="h-3.5 w-3.5 animate-spin" /> : null}
            {saving ? "Saving" : "Record adjustment"}
          </button>
        </footer>
      </div>
    </div>
  );
}

/** The reason codes, in the module's own order. Kept here so the drawer and the spec agree. */
const REASONS = [
  { value: "correction", label: "Correction" },
  { value: "damage", label: "Damage" },
  { value: "loss", label: "Loss" },
  { value: "purchase_receipt", label: "Purchase receipt" },
  { value: "customer_return", label: "Customer return" },
  { value: "sale_shipment", label: "Sale shipment" },
  { value: "supplier_return", label: "Return to supplier" },
  { value: "internal_use", label: "Internal use" },
  { value: "stocktake_variance", label: "Stocktake variance" },
];

/**
 * The scanner box.
 *
 * A scanner types a barcode and presses Enter, so this is a real form with a real submit rather
 * than an input with an `onKeyDown`: a hardware scanner that fails to be recognised is the one
 * failure mode this control exists to prevent, and a form is the widest target it can have.
 */
export function ScannerBox({
  onResolved,
  onMiss,
}: {
  onResolved: (itemId: string, sku: string, name: string) => void;
  onMiss?: (code: string) => void;
}) {
  const [code, setCode] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function lookup(event: React.FormEvent) {
    event.preventDefault();
    const typed = code.trim();
    if (!typed) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      const { lookupItem } = await import("@/lib/inventory");
      const item = await lookupItem(typed);
      onResolved(item.id, item.sku, item.name);
      setCode("");
    } catch {
      // The server answers a miss with a 404, so "not found" is the honest message and the code
      // stays in the box — a warehouse worker scanning a damaged label retypes it once, and
      // emptying the field makes them scan the whole stack again to find out what they had.
      setError(`Nothing is labelled “${typed}”.`);
      onMiss?.(typed);
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={lookup} className="flex items-end gap-2" data-qa-inventory-scanner>
      <label className="flex-1 text-[12px] font-medium">
        <span className="inline-flex items-center gap-1.5">
          <ScanLine aria-hidden className="h-3.5 w-3.5" />
          Scan a barcode
        </span>
        <input
          data-qa-inventory-scanner-input
          value={code}
          onChange={(event) => setCode(event.target.value)}
          // Scanners emit the digits and often a trailing Enter as one keystroke burst, so the
          // field takes focus and waits rather than filtering on every character.
          placeholder="Scan, or type a SKU and press Enter"
          className="mt-1 w-full rounded border px-2 py-1.5 font-mono text-[13px]"
        />
      </label>
      <button
        type="submit"
        disabled={busy || !code.trim()}
        className="rounded border px-3 py-1.5 text-[13px] disabled:opacity-50"
      >
        {busy ? "Looking…" : "Find"}
      </button>
      {error ? (
        <p className="text-[12px] text-red-700" data-qa-inventory-scanner-miss>
          {error}
        </p>
      ) : null}
    </form>
  );
}

/** A stock row, as the stock list and the item detail both render it. */
export function StockRowLine({ row }: { row: StockLevel }) {
  return (
    <div className="flex items-center justify-between gap-3 border-b py-2 last:border-b-0">
      <div className="min-w-0">
        <p className="truncate text-[13px] font-medium">{row.name}</p>
        <p className="text-[12px] text-muted">
          {row.sku} · {row.warehouse_code}/{row.location_code}
        </p>
      </div>
      <div className="flex shrink-0 items-center gap-3">
        <QuantityCell value={row.on_hand} />
        <StockStatusBadge status={row.status} />
      </div>
    </div>
  );
}

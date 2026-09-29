import { QuoteBuilderView } from "@/features/sales/quote-builder-view";

/**
 * `/sales/quotes/new` — the builder, creating a draft.
 *
 * The same component as `/sales/quotes/{id}/edit`; without an id it posts the whole document
 * through `POST /sales/quotes` rather than replacing the grid of an existing one.
 */
export default function Page() {
  return <QuoteBuilderView />;
}

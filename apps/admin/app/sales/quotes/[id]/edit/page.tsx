import { QuoteBuilderView } from "@/features/sales/quote-builder-view";

/**
 * `/sales/quotes/{id}/edit` — the grid, editing a quote that is still a working document.
 *
 * A separate route from `/sales/quotes/new` rather than the same one with a query parameter: the
 * two screens save through different endpoints (`POST /quotes` creates, `PUT /quotes/{id}/lines`
 * replaces), and a parameter would make one screen branch on "am I creating or editing" in every
 * save. Two routes, one component, no branch.
 */
export default async function Page({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return <QuoteBuilderView quoteId={id} />;
}

import { QuoteDetailView } from "@/features/sales/quote-detail-view";

/**
 * `/sales/quotes/{id}` — the document, its versions, and the customer's link.
 *
 * The id arrives as a promise in Next 15's App Router, so the page is `async` and awaits it rather
 * than reaching for `useParams()`: a page that is a client component cannot do the first without
 * giving up the second.
 */
export default async function Page({ params }: { params: Promise<{ id: string }> }) {
  const { id } = await params;
  return <QuoteDetailView quoteId={id} />;
}

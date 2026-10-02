import { PublicQuoteView } from "@/features/sales/public-quote-view";

/**
 * `/q/{token}` — the customer's copy of a quote.
 *
 * The route lives at the site root rather than under `/sales`, because the person opening it is the
 * customer: a link that says `/sales/quotes/...` tells them they are in somebody's internal panel,
 * and the token in it is the only credential they have.
 */
export default async function Page({ params }: { params: Promise<{ token: string }> }) {
  const { token } = await params;
  return <PublicQuoteView token={token} />;
}

import { StocktakeScreen } from "@/features/inventory/stocktake-view";

/**
 * `/inventory/stocktake` (REQ-053, slice 4).
 *
 * **One route, two views, chosen by a query parameter** rather than by a second path. The list
 * and the sheet are two states of one document, and a person who follows a link to a specific
 * count should land on that count — which `?id=` does and `/inventory/stocktake/ST-0001` would
 * not, because the number is a label and the id is the identity.
 *
 * The switch is a small client component because the sheet owns client state (the count boxes)
 * and a server component would have to re-render the whole route to save one number. The page
 * itself stays a server component, so nothing about the route is forced to the client.
 */
export default function Page() {
  return <StocktakeScreen />;
}

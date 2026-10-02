import { ItemDetailView } from "@/features/inventory/item-detail-view";

/**
 * `/inventory/items/{id}` (REQ-053).
 *
 * The route four screens in the module already linked to and none of them could reach: the stock
 * list (both its table and its cards), the movements ledger and the approvals inbox all render the
 * item's name as a `Link` to this path, and `apps/admin/app/inventory/` had no `items` directory,
 * so every one of them was a dead destination.
 */
export default function ItemDetailPage() {
  return <ItemDetailView />;
}
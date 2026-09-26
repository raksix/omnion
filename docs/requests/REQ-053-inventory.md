> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/inventory`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Stock across one or many warehouses.

- **Items** (SKU, barcode, unit, category, min/max thresholds, reorder point).
- **Locations & warehouses** with per-location stock, transfers between locations.
- **Movements ledger** (receipt, issue, transfer, adjustment) — append-only with reason codes.
- **Low-stock alerts** producing notifications + optional automation rule.
- **Counting / stocktake** session with variance report.
- **Views**: stock list with filters (below threshold, negative, idle), item detail with movement history.
- **Events**: `inventory.item.created`, `inventory.stock.low`, `inventory.movement.recorded`.

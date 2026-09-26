> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/manufacturing`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Light manufacturing for product businesses.

- **Bills of materials** (BoM) with components, quantities, waste factor, per finished item.
- **Work orders**: create from a BoM (manually or from a sales order), states (planned/started/done), assignee.
- **Component consumption & output** posts inventory movements automatically.
- **Shop-floor view**: today's work orders, start/complete with quantity.
- **Costing-lite**: material + time estimate vs. actual.
- **Events**: `manufacturing.workorder.created`, `manufacturing.workorder.completed`.

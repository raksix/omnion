# REQ-026 — Dynamic Data Model

> **Status:** pending · **Captured:** 2026-09-25 · **Layer:** core (dynamic entity layer)
> **Source:** owner brief — platform periphery & headline features (2026-09-25)

## Request

Users can create their own entities:

```text
Create Entity

Name:
Vehicle

Fields:
├── Plate
├── Brand
├── Model
├── Year
└── Employee
```

Omnion then creates automatically:

```text
Vehicle API
Vehicle Admin UI
Vehicle Permissions
Vehicle Search
Vehicle Audit
Vehicle Workflow Events
```

## Notes

- Foundation for the App Builder (REQ-025); must emit workflow events (REQ-016) and
  audit entries (REQ-039) for every generated entity from day one.

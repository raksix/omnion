> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/hr`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

People operations.

- **Employees** (profile, position, department, manager, start date, employment type) with documents.
- **Departments & org chart** (tree view).
- **Leave**: types (annual, sick, unpaid), balances, request → approve flow, calendar of absences.
- **Attendance**: check-in/out (manual or via API), monthly summary.
- **Onboarding checklist** templates applied per new employee.
- **Permissions**: HR role sees all; manager sees own team; employee sees self.
- **Events**: `hr.leave.requested`, `hr.leave.approved`, `hr.employee.joined`.

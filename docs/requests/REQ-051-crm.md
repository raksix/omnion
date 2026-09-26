> **Status:** pending · **Captured:** 2026-09-26 · **Layer:** module (`modules/crm`)
> **Source:** owner brief — business suite / frontend depth (docs/08-BUSINESS-SUITE.md, docs/03-FRONTEND.md)

## Request

Relationship layer of the platform: people and companies, the deals they are part of, and the activity trail around them.

- **Contacts** (people) and **Companies** with custom fields, tags, owners, and merge/dedupe.
- **Deals** with stages, amount, probability, expected close date, owner, and a **pipeline board** (drag between stages, per-stage totals).
- **Activities** (call, meeting, note, task) attached to contacts/deals, with a timeline view.
- **Lists & filters**: saved views ("my open deals", "stale this month"), column chooser, inline edit.
- **Import/export** of contacts (CSV) with mapping preview.
- **Permissions**: per-role visibility (own / team / all), field-level hiding for sensitive fields.
- **Automation hooks**: events (`crm.contact.created`, `crm.deal.stage_changed`) feeding the workflow engine.
- **AI copilot**: summarize a deal, draft a follow-up mail, suggest next action (uses AI Hub).

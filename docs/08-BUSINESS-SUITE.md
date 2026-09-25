# Omnion — Business Suite (Odoo-style Modules)

> Captured from the owner's brief (2026-09-25, part 8). The core idea: CMS + enterprise
> platform + **Odoo-style business modules** — without embedding Odoo's features into the Core:
>
> **Omnion Core = the operating system. Modules = the Odoo-style applications.**

## The module ecosystem

```text
modules/
├── cms/
├── website/
├── blog/
├── ecommerce/
│
├── crm/
├── sales/
├── purchases/
├── inventory/
├── accounting/
├── invoicing/
├── subscriptions/
│
├── projects/
├── tasks/
├── timesheets/
├── helpdesk/
├── appointments/
│
├── hr/
├── recruitment/
├── employees/
├── leaves/
├── expenses/
│
├── marketing/
├── email-marketing/
├── social/
├── events/
│
├── documents/
├── knowledge/
├── approvals/
│
└── manufacturing/
```

All of them use the **same Omnion Core**.

## CRM

Full CRM:

```text
Leads
 ↓
Opportunities
 ↓
Pipeline
 ↓
Quotation
 ↓
Sale
 ↓
Customer
```

Pipeline:

```text
New
 ↓
Qualified
 ↓
Proposal
 ↓
Negotiation
 ↓
Won / Lost
```

Kanban view. AI plugs in here too:

> "Show me the opportunities most likely to close this month."

## Sales

```text
Quotation
    ↓
Sales Order
    ↓
Invoice
    ↓
Payment
```

PDF quote generation. Customer portal:

```text
Customer Portal
├── Quotes
├── Orders
├── Invoices
├── Payments
└── Tickets
```

## Inventory

Odoo-style stock system:

```text
Warehouse
│
├── Locations
├── Products
├── Stock Moves
├── Transfers
├── Receipts
└── Deliveries
```

Example:

```text
Warehouse A
├── Raw Materials
├── Finished Products
└── Returns
```

Keeping stock movements in a **ledger** model is very important.

## Manufacturing

This can grow considerably later:

```text
Product
 ↓
Bill of Materials
 ↓
Manufacturing Order
 ↓
Work Orders
 ↓
Finished Product
```

Work centers:

```text
Work Center
├── Machine
├── Capacity
├── Employees
└── Operations
```

## Accounting

Careful here: accounting varies heavily by country. So:

```text
modules/accounting-core
```

and:

```text
modules/accounting-localizations/
├── tr/
├── de/
├── uk/
├── us/
└── ...
```

For Türkiye specifically, integrations such as these can be added as
module/localization pieces:

- e-Fatura
- e-Arşiv
- e-İrsaliye
- KDV (VAT)
- tax rules
- fiscal periods
- local reports

## HR

```text
Employees
├── Employee profiles
├── Departments
├── Positions
├── Contracts
├── Attendance
├── Leave
├── Expenses
└── Recruitment
```

Employee portal:

```text
My Profile
My Leaves
My Expenses
My Documents
My Tasks
```

## Project Management

Trello + Jira style:

```text
Project
│
├── Tasks
├── Milestones
├── Members
├── Files
├── Timesheets
└── Discussions
```

Task:

```text
TODO
 ↓
IN PROGRESS
 ↓
REVIEW
 ↓
DONE
```

Kanban + List + Calendar + Gantt views.

## Helpdesk

```text
Customer
 ↓
Ticket
 ↓
Team
 ↓
Agent
 ↓
Resolution
```

SLA:

```text
Critical → 1 hour
High     → 4 hours
Normal   → 24 hours
```

AI:

> "Analyze this ticket, find the related previous tickets, and draft a reply."

## Marketing

Odoo-style:

```text
Marketing
├── Campaigns
├── Email Marketing
├── SMS
├── Social
├── Forms
├── Landing Pages
└── Automations
```

Example automation:

```text
Form submitted
       ↓
CRM Lead
       ↓
Email
       ↓
Wait 2 days
       ↓
If opened
       ↓
Sales notification
```

## Calendar / Appointments

```text
Calendar
├── Meetings
├── Events
├── Appointments
└── Resources
```

Example:

```text
Book a meeting
 ↓
Available slots
 ↓
Calendar
 ↓
Email confirmation
```

## Documents

Internal DMS:

```text
Documents
├── Contracts
├── HR
├── Finance
├── Projects
└── Legal
```

Versioning + approval + access control.

## Knowledge

Notion/Wiki-like:

```text
Knowledge
├── Company Wiki
├── Documentation
├── Policies
├── Procedures
└── Internal Knowledge
```

And **the AI Hub can build RAG on top of it**:

> "Find the company's leave policy."

AI:

```text
Knowledge Base
 ↓
Relevant documents
 ↓
Answer
 ↓
Source citations
```

## Approvals

Internal approval system:

```text
Employee
 ↓
Expense Request
 ↓
Manager
 ↓
Finance
 ↓
Approved
```

Built on the generic workflow engine, so:

```text
Expense Approval
Purchase Approval
Leave Approval
Deployment Approval
Content Approval
```

all use the same infrastructure.

## Automation

What connects all these modules:

```text
Event
 ↓
Trigger
 ↓
Condition
 ↓
Action
```

Example:

```text
Invoice overdue
       ↓
Wait 3 days
       ↓
Send email
       ↓
Create task
       ↓
Notify finance
```

## Website + Business, combined

This is where Omnion's real differentiator shows. A company creates its website:

```text
omnion.com
```

and then manages all of this from the same panel:

```text
Website
CRM
Sales
Inventory
Accounting
Helpdesk
Projects
HR
Marketing
Documents
AI
```

A **"Request a Quote"** form on the website:

```text
Website Form
     ↓
CRM Lead
     ↓
Sales Opportunity
     ↓
Quotation
     ↓
Customer
```

## AI over all modules

For example, the admin says:

> "Find the customers with overdue invoices this month, create tasks for their account owners in CRM, and prepare professional email drafts to send to the customers."

AI:

```text
Accounting
    ↓
Find overdue invoices
    ↓
CRM
    ↓
Find account owners
    ↓
Tasks
    ↓
Create tasks
    ↓
AI
    ↓
Generate email drafts
    ↓
Approval
```

**This is the point where the AI Hub truly earns its place.**

## Shared infrastructure

Modules are not disconnected from each other:

```text
                    OMNION CORE
                         │
     ┌───────────────────┼───────────────────┐
     │                   │                   │
 Identity            Workflow              AI Hub
     │                   │                   │
     └───────────────────┼───────────────────┘
                         │
                    Business Data
                         │
 ┌────────┬────────┬─────┼──────┬────────┬────────┐
 CRM    Sales   Inventory  Accounting   HR   Helpdesk
 └────────┴────────┴─────┼──────┴────────┴────────┘
                         │
                       CMS
                         │
                      Website
```

And **no module re-implements the same things.** For example:

- User system → Core
- Permission → Core
- Workflow → Core
- Notifications → Core
- Files → Core
- Audit → Core
- AI → AI Hub
- Search → Core
- Events → Core

Business modules consume these.

## Three layers

```text
OMNION
│
├── CORE
│   ├── Identity
│   ├── Permissions
│   ├── Workflow
│   ├── Files
│   ├── Notifications
│   ├── Audit
│   ├── Search
│   ├── AI Hub
│   └── Automation
│
├── PLATFORM
│   ├── CMS
│   ├── Website
│   ├── Marketplace
│   ├── Themes
│   ├── Plugins
│   └── Developer Platform
│
└── BUSINESS SUITE
    ├── CRM
    ├── Sales
    ├── Accounting
    ├── Inventory
    ├── Manufacturing
    ├── HR
    ├── Projects
    ├── Helpdesk
    ├── Marketing
    ├── Documents
    └── Knowledge
```

So someone who only wants the CMS does not have to install CRM/Accounting. The Business Suite
is switched on from the Marketplace or the module manager when desired.

And because all of these modules use the same **Role/Permission, Workflow, AI, Automation,
Audit, Notification, Search and File** infrastructure, the system will not behave like 50
disconnected apps even when it grows as large as Odoo.

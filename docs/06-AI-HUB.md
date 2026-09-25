# Omnion — AI Hub

> Captured from the owner's brief (2026-09-25, part 6). The AI Hub is one of Omnion's most
> important pieces — and it is not "ChatGPT bolted onto the admin": **the AI is a controlled
> operator of the Omnion API.**

## Overview

```text
                         OMNION AI HUB
                              │
             ┌────────────────┼────────────────┐
             │                │                │
         Providers          Models           Agents
             │                │                │
      ┌──────┴──────┐    ┌────┴────┐      ┌───┴────┐
      │             │    │         │      │        │
   OpenAI       Anthropic  GPT    Claude  CMS Agent Admin Agent
   OpenCode     Google     ...     ...    SEO Agent AI Agent
   CommandCode  Ollama
   Custom       ...
```

## 1. Users can add their own AI provider

Admin:

```text
AI Hub
└── Providers
    ├── OpenAI
    ├── Anthropic
    ├── Google
    ├── OpenCode
    ├── CommandCode
    ├── Ollama
    └── Custom
```

**Custom Provider** is especially important. The user enters:

```text
Provider Name:
My Local AI

Base URL:
https://ai.example.com/v1

API Key:
************

Protocol:
OpenAI Compatible

Models:
- model-a
- model-b
```

This way, almost any service exposing an OpenAI-compatible API can connect.

## 2. Provider abstraction

Backend:

```text
AIProvider
├── chat()
├── stream()
├── embeddings()
├── generate_image()
├── generate_audio()
├── transcribe()
└── list_models()
```

Implementations:

```text
OpenAIProvider
AnthropicProvider
GoogleProvider
OllamaProvider
OpenCodeProvider
CustomOpenAIProvider
```

But the rest of Omnion **does not know which provider is in use.** It just calls:

```text
ai.chat(...)
```

The AI Hub decides:

```text
AI Request
    ↓
Model Router
    ↓
OpenAI / Claude / Gemini / Local
```

## 3. Model Registry (instead of a provider-centric view)

```text
AI Hub
└── Models

GPT-5.x
Claude
Gemini
Llama
Qwen
Mistral
DeepSeek
Custom Model
```

Each model carries metadata:

```json
{
  "id": "provider/model",
  "context_window": 200000,
  "supports_tools": true,
  "supports_vision": true,
  "supports_streaming": true,
  "supports_embeddings": false
}
```

So the system knows what a model can do.

## 4. Model Router

**This part is very important.** The user can pick a default:

```text
Default AI Model:
GPT
```

But Omnion can use different models for different jobs:

```text
Simple text       → cheap model
Translation       → translation model
Code generation   → coding model
Vision            → vision model
Huge document     → long-context model
Embeddings        → embedding model
```

More advanced:

```text
AI Router

if task == "coding":
    use coding-model

if task == "translation":
    use translation-model

if task == "cheap":
    use cheap-model

if task == "critical":
    use best-model
```

The user can change all of this from the UI.

## 5. The main event: AI Agents

The AI does not merely answer — it operates:

```text
User
 ↓
AI
 ↓
Tool / Permission System
 ↓
Omnion
```

Example — the admin says:

> "Change the hero title on the homepage, update the SEO description, and send it to staging."

The AI:

```text
1. Find the homepage
2. Read the existing content
3. Change the hero
4. Generate the SEO description
5. Create a revision
6. Deploy to staging
7. Report the result
```

## 6. Tool System

Instead of giving the AI direct database access, we give it **tools**:

```text
Tools
├── content.search
├── content.read
├── content.create
├── content.update
├── content.publish
├── content.rollback
│
├── media.search
├── media.upload
│
├── users.search
├── users.create
│
├── site.get
├── site.update
│
├── theme.list
├── theme.activate
│
├── plugin.list
├── plugin.install
│
├── workflow.start
│
├── analytics.query
│
├── deployment.preview
└── deployment.deploy
```

The AI can use these.

## 7. Permission system

**We do not give the AI unlimited power. This is very important.**

```text
AI Agent Permissions

Content
☑ Read
☑ Create
☑ Update
☐ Delete
☐ Publish

Users
☑ Read
☐ Create
☐ Delete

Plugins
☑ Read
☐ Install
☐ Uninstall

Deployment
☐ Deploy
```

The AI can behave like an admin — but **its permissions are controlled separately.**

## 8. Separate RBAC for AI

For example:

```text
AI Role: Content Assistant
```

gets only:

```text
content.read
content.create
content.update
seo.analyze
media.search
```

Another:

```text
AI Role: DevOps Agent
```

gets:

```text
deployment.read
deployment.restart
logs.read
health.read
```

## 9. Approval system

Dangerous operations are not executed directly. Example:

> "Delete all users."

AI:

```text
⚠ Dangerous Action

Delete 14,823 users?

[Cancel]
[Approve]
```

The same applies to:

- publish
- delete
- plugin install
- theme change
- deployment
- database operation

## 10. AI Action Preview

The AI shows its work before doing it:

```text
Proposed Changes

Homepage
├── Hero title
│   OLD: Welcome
│   NEW: Welcome to Omnion
│
├── SEO title
│   OLD: Company
│   NEW: Company | Omnion
│
└── Meta description
    MODIFIED

[Reject] [Approve]
```

Approve executes it.

## 11. AI Conversation → real operations

Example:

> "Look at last week's analytics. Find the top 5 most-visited pages and fix the ones with weak SEO."

AI:

```text
Analytics Tool
      ↓
5 pages found
      ↓
SEO Analyzer
      ↓
3 pages need improvement
      ↓
Content Tools
      ↓
Generate changes
      ↓
Approval
      ↓
Publish
```

This is no longer a chatbot — it is a **platform operator**.

## 12. AI Memory

Not every conversation has to be isolated. Memory scopes:

```text
Global AI Memory
Organization Memory
Site Memory
User Memory
Conversation Memory
```

For example, a site's AI knows:

```text
Brand name: Acme
Tone: Professional
Primary language: Turkish
Target audience: Enterprise
```

## 13. Knowledge Base / RAG

AI Hub:

```text
Knowledge
├── Documents
├── Pages
├── PDFs
├── FAQs
├── Internal Docs
└── Website Content
```

Pipeline:

```text
Documents
 ↓
Chunking
 ↓
Embeddings
 ↓
Vector Store
 ↓
RAG
 ↓
AI
```

So when someone asks "What is our refund policy?", it answers from the company's own content.

## 14. AI Agents Marketplace

The Marketplace gets a dedicated category:

```text
AI Agents
```

Examples:

```text
SEO Agent
CRM Agent
Support Agent
Translation Agent
Content Agent
Analytics Agent
Security Agent
DevOps Agent
```

The user installs — e.g. `Install SEO Agent` — and the agent requests the tool permissions it needs.

## 15. Multi-Agent system

At a more advanced level:

```text
                    AI Orchestrator
                           │
        ┌──────────────────┼──────────────────┐
        ↓                  ↓                  ↓
   SEO Agent          Content Agent      Analytics Agent
        │                  │                  │
        └──────────────────┼──────────────────┘
                           ↓
                      Final Result
```

Example — "prepare a new product campaign":

```text
Content Agent
      ↓
SEO Agent
      ↓
Image Agent
      ↓
Translation Agent
      ↓
Review Agent
```

## 16. AI Cost Manager

A must-have for enterprise:

```text
AI Usage

OpenAI
$42.31

Anthropic
$17.22

Local
$0

Total
$59.53
```

Also limits:

```text
Organization limit:
$500 / month
```

Per site:

```text
Site A → $120
Site B → $80
Site C → $32
```

## 17. AI Logs

Every AI operation:

```text
AI Audit

User:
admin

Agent:
Content Agent

Model:
GPT

Action:
content.update

Resource:
page/123

Tokens:
12,432

Cost:
$0.04

Result:
Success
```

Especially important for enterprise.

## 18. Privacy / Data Policy

Per provider:

```text
Send user data:
☐ Allowed

Send media:
☐ Allowed

Send analytics:
☐ Allowed

Send private content:
☐ Allowed
```

Plus an **AI Data Guard** layer:

```text
PII Detection
   ↓
Mask
   ↓
Send to AI
```

Example — instead of:

```text
email@example.com
```

send:

```text
[EMAIL_1]
```

and map it back when the result returns, if needed.

## 19. Local AI

For enterprise customers:

```text
Ollama
vLLM
Local OpenAI-compatible endpoint
```

can be connected. So:

```text
Internet
   X
   │
Omnion
   │
Local GPU
   ↓
LLM
```

Your data never leaves — AI runs fully locally when required.

## 20. The AI Hub admin screen

```text
AI HUB
────────────────────────────────────

Overview

AI Status              ● Operational

Default Model          GPT
Fallback Model         Claude

Today's Requests       2,481
Tokens                 8.4M
Cost                   $12.43

────────────────────────────────────

Providers

✓ OpenAI
✓ Anthropic
✓ Google
✓ Ollama
+ Add Provider

────────────────────────────────────

Models

GPT
Claude
Gemini
Llama
Qwen

────────────────────────────────────

Agents

Content Agent
SEO Agent
Analytics Agent
Support Agent

────────────────────────────────────

Knowledge

Documents
Vector Index
Sources

────────────────────────────────────

Security

Permissions
Data Policy
Audit Logs

────────────────────────────────────

Usage

Requests
Tokens
Costs
Limits
```

## Positioning

```text
                     ┌────────────────────┐
                     │      AI HUB        │
                     └─────────┬──────────┘
                               │
              ┌────────────────┼────────────────┐
              │                │                │
           Provider          Router           Memory
              │                │                │
        ┌─────┴─────┐          │             RAG
        │           │          │              │
      OpenAI      Local       Model          KB
      Claude      Ollama      Selection
      Gemini      Custom
              │
              ▼
        ┌───────────────┐
        │ Agent Runtime │
        └───────┬───────┘
                │
        ┌───────▼────────┐
        │ Tool / Policy  │
        │    Engine      │
        └───────┬────────┘
                │
       ┌────────┼──────────┐
       ▼        ▼          ▼
      CMS     Users      Plugins
       │        │          │
       └────────┼──────────┘
                ▼
             OMNION
```

**The provider can change, but the interface the AI uses to talk to Omnion does not.** One
customer uses OpenAI, another Claude, a third runs their own Llama/Qwen/vLLM server — for
Omnion, all of them flow through the same AI Hub.

And the chain **`AI Agent → Tool → Permission → Approval → Audit`** is meant to be Omnion's
core security architecture. What elevates "the AI can manage the whole system" to enterprise
grade is not the model — it is this layer that controls what the AI may do, with which
permissions.

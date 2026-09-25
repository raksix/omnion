# Omnion — Project Context

Living context document: what Omnion is, the owner's directives, and open questions.
Updated continuously — new directives/decisions get appended here as they arrive.
Chat is Turkish; this document is English.

_Last updated: 2026-09-25_

## Phase

- **Context gathering. No implementation — yet.** The owner paused building on 2026-09-25
  ("we are not building the project now; we'll gather context"). Do not scaffold
  stack choices, features or code until the owner asks to start.

## What Omnion is

- Omnion is a **CMS (content management system) panel**.
- Public framing: everywhere and always, the project is described only as a CMS panel.

## Hard rules (owner directives)

1. **General-audience, professional wording only.** This repository is public. Every word that
   ships with it — README, docs, code, comments, commit messages, repo description, asset
   names — must be plain, neutral CMS/product language appropriate for a general audience.
   Nothing suggestive, edgy or informal goes in, even as a placeholder or joke.
2. **Public repo hygiene.** Never commit secrets, credentials, API keys or heavy media
   files; media belongs on the app's own storage/CDN.
3. **Language split.** Chat in Turkish; documentation, code comments and commit messages
   in English.
4. **Commits.** Small, atomic, imperative, English. Push immediately after every change.
5. **Context first.** Capture directives in this file as they arrive; build only when the
   owner asks to start.

## Directives log (append-only)

| Date | Directive (owner) | Applied as |
|---|---|---|
| 2026-09-25 | Omnion is a general-audience CMS panel; the public repo must never carry language outside that framing; clean up any wording that violates it and re-push. | README wording, commit history and repository description cleaned; history rewritten; the previous repo was privatized as `raksix/omnion-legacy`; a fresh public repo was published and pushed. Rule 1 recorded. |
| 2026-09-25 | "We are not building the project now — we'll gather context; just save my directives into docs." | This file created; build paused. |

## Open questions (to resolve while gathering context)

- What content will the panel manage (content types, fields, workflows)?
- Who uses it (roles, permissions, multi-user)?
- Stack, hosting and integrations?
- Scope: admin panel only, or panel + public-facing side?
- Naming/branding details beyond "Omnion".

## How to work with this file

1. Apply the directive.
2. Add a row to the directives log.
3. If it changes how we work, update "Hard rules".

# Omnion — Frontend & Theme System

> Captured from the owner's brief (2026-09-25, part 3). Frontend direction: **Next.js + React +
> TypeScript**, with a theme system designed to be more modern than WordPress's.

## Omnion frontend overview

```text
Omnion
├── Admin Panel
│   └── Next.js + React + TypeScript
│
└── Public Websites
    └── Next.js + Theme Engine
```

Important separation: **the admin panel and the public-site frontends do not have to be the
same application.**

## Ten default themes

A fresh installation ships with **10 professional themes**:

1. **Corporate** — corporate company
2. **Tech** — technology/SaaS
3. **Agency** — agency
4. **Portfolio** — personal/portfolio
5. **Magazine** — news/blog
6. **Commerce** — e-commerce
7. **Documentation** — docs
8. **Startup** — startup
9. **Minimal** — minimal
10. **Government** — public-sector/corporate portal

And these must not be color-swapped templates. Each one has its own:

- layout
- components
- responsive design
- dark/light mode
- typography system
- header/footer options
- hero sections
- card systems
- blog design
- menu structures

## WordPress-style theme system

From the admin panel, users see:

```text
Appearance
│
├── Themes
│
├── Active Theme
│
├── Customize
│
├── Widgets / Blocks
│
└── Theme Upload
```

A theme gallery:

```text
┌──────────────────────────────────────────┐
│ Themes                                   │
├──────────────────────────────────────────┤
│                                          │
│  [Corporate]    [Tech]      [Agency]    │
│                                          │
│  [Startup]      [Minimal]   [Magazine]  │
│                                          │
│  [Commerce]     [Docs]      [Portfolio] │
│                                          │
│             [Government]                 │
└──────────────────────────────────────────┘
```

With a **Preview → Install → Activate** flow.

## The real highlight: Theme Builder

Users should not have to write code. Under `Appearance → Customize`, they can visually arrange:

```text
Header
 ├── Logo
 ├── Navigation
 ├── Search
 └── CTA

Homepage
 ├── Hero
 ├── Features
 ├── Testimonials
 ├── Pricing
 └── CTA

Footer
 ├── Columns
 ├── Social
 └── Copyright
```

## Block system

Take WordPress's Gutenberg concept and modernize it:

```text
Blocks
├── Hero
├── Text
├── Image
├── Gallery
├── Video
├── Button
├── Cards
├── FAQ
├── Testimonials
├── Pricing
├── Contact Form
├── Map
├── Blog
├── Products
└── Custom HTML
```

The user clicks **+ Block** and adds it to the page.

## Theme is not content (critical)

The ideal data flow:

```text
CONTENT
   ↓
Block / Component Data
   ↓
THEME
   ↓
Rendered Website
```

When a user switches themes, "my content disappeared" must never happen:

```text
Site
 ├── Content
 │    ├── Pages
 │    ├── Posts
 │    └── Media
 │
 └── Presentation
      ├── Theme
      ├── Colors
      ├── Fonts
      └── Layout
```

This separation is **very important**.

## Theme developer SDK

A developer can run:

```text
omnion create-theme my-theme
```

and get:

```text
my-theme/
├── omnion.theme.json
├── layouts/
├── pages/
├── components/
├── blocks/
├── assets/
├── styles/
└── locales/
```

with access to the theme API:

```text
site()
page()
menu()
posts()
media()
translations()
settings()
```

## Design quality bar

"Ten themes, but they all look like 2014 WordPress themes" must not happen. Default themes
ship with:

- modern SaaS design
- solid typography
- mobile-first
- accessibility
- dark mode
- tasteful animations
- responsive grid
- modern navigation
- skeleton/loading states
- SEO
- Open Graph
- structured data

Shared component system:

```text
@omnion/ui
├── Button
├── Input
├── Modal
├── Dropdown
├── Tabs
├── Card
├── Table
├── Dialog
├── Toast
└── ...
```

Themes can **freely override** these components.

## Final frontend architecture

```text
                    OMNION
                       │
              ┌────────┴────────┐
              │                 │
         Admin Panel       Public Frontend
              │                 │
           Next.js            Next.js
              │                 │
           React             Theme Engine
              │                 │
        Omnion UI         ┌─────┴─────┐
                          │           │
                       Theme 1 ... Theme 10
                          │
                       Blocks
                          │
                       Content API
                          │
                      Rust Core
```

**So yes: Next.js is certain.** The admin panel is Next.js, and the default public themes can
also be Next.js-based.

## Marketplace (future)

If Omnion is designed so that an **"Omnion Marketplace"** can open later, third-party
developers can sell or freely distribute themes/plugins — a structure that can significantly
grow the open-source project's ecosystem.

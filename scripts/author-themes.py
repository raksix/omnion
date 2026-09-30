#!/usr/bin/env python3
"""
Write the nine remaining bundled themes (REQ-062 slice 4).

This is a ONE-OFF authoring script, not a runtime dependency: it exists because the nine
themes differ in MARKUP and TOKENS — which is exactly the part that must not be templated —
and writing them by hand would mean nine opportunities to retype a file that should have been
identical. The shared parts (manifest, package.json, theme.ts, index.ts) really are identical
and really should be generated. The layout and the stylesheet are written per theme below and
are the part a reviewer reads.

`omnion create-theme` is the runtime scaffolder for a THIRD-party theme; this script is how
the platform authors its own nine, because a bundled theme ships to a quality bar
(docs/03-FRONTEND.md) that a skeleton deliberately does not reach.

Run from the repository root:  python3 scripts/author-themes.py
"""
from __future__ import annotations

import json
import pathlib
import shutil
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
THEMES = ROOT / "themes"

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from theme_source import (  # noqa: E402
    class_prefix,
    layout_tsx,
    preview_svg,
    stylesheet,
)


# The eight layout slots docs/03-FRONTEND.md §"Ten default themes" describes. Every bundled
# theme ships the same slot set, so the Theme Builder's picker means the same thing in all of
# them; what differs is the MARKUP each slot draws and the tokens behind it.
SLOTS = ["header", "footer", "home", "blog-list", "single", "product", "404", "search"]


# ---------------------------------------------------------------------------------------------
# Per-theme design data. Every field here is a design decision, not a variable.
# ---------------------------------------------------------------------------------------------

THEME_DATA: list[dict] = [
    {
        "key": "corporate",
        "name": "Corporate",
        "description": "An institutional company site: a full-width masthead, a narrow reading column and section rules.",
        "audience": "company",
        "layout": "masthead",
        "font_body": 'ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif',
        "font_title": 'ui-sans-serif, system-ui, "Segoe UI", Roboto, sans-serif',
        "measure": "40rem",
        "title_scale": "clamp(2.1rem, 1.4rem + 2.6vw, 3.1rem)",
        "title_weight": "700",
        "title_tracking": "-0.025em",
        "title_transform": "none",
        "radius": "2px",
        "base_size": "17px",
        "header_style": "band",
        "cards": "bordered",
        "tokens": {
            "canvas": ("#ffffff", "#12161c"),
            "surface": ("#f6f7f9", "#1a1f27"),
            "ink": ("#14181e", "#eef1f5"),
            "muted": ("#5b6472", "#98a2b3"),
            "line": ("#dfe3e9", "#2a313c"),
            "accent": ("#0b4fa8", "#7cb0ff"),
            "accent_soft": ("#e8f0fb", "#16273d"),
        },
        "aliases": ["business", "enterprise"],
    },
    {
        "key": "tech",
        "name": "Tech",
        "description": "A product site for software: a dark-canvas hero, a mono accent line and a dense type scale.",
        "audience": "saas",
        "layout": "hero",
        "font_body": 'ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif',
        "font_title": 'ui-sans-serif, system-ui, "Segoe UI", Roboto, sans-serif',
        "measure": "46rem",
        "title_scale": "clamp(2.3rem, 1.5rem + 3.2vw, 3.8rem)",
        "title_weight": "750",
        "title_tracking": "-0.04em",
        "title_transform": "none",
        "radius": "10px",
        "base_size": "16px",
        "header_style": "bar",
        "cards": "elevated",
        "tokens": {
            "canvas": ("#fbfcfe", "#0b0e14"),
            "surface": ("#f2f5fa", "#141a25"),
            "ink": ("#0d1117", "#e8edf5"),
            "muted": ("#57606f", "#8b96a8"),
            "line": ("#e1e6ee", "#222b39"),
            "accent": ("#5b3df5", "#a394ff"),
            "accent_soft": ("#eeecfe", "#1c1a3a"),
        },
        "aliases": ["saas", "startup-soft"],
    },
    {
        "key": "agency",
        "name": "Agency",
        "description": "A studio site: an oversized display line, wide gutters and a card grid that breaks the measure.",
        "audience": "studio",
        "layout": "wide",
        "font_body": 'ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif',
        "font_title": 'ui-sans-serif, system-ui, "Segoe UI", Roboto, sans-serif',
        "measure": "38rem",
        "title_scale": "clamp(2.6rem, 1.2rem + 5vw, 5rem)",
        "title_weight": "800",
        "title_tracking": "-0.05em",
        "title_transform": "none",
        "radius": "4px",
        "base_size": "18px",
        "header_style": "bare",
        "cards": "outline",
        "tokens": {
            "canvas": ("#fffdf9", "#141210"),
            "surface": ("#f6f1e8", "#201c17"),
            "ink": ("#1b1815", "#f2ede4"),
            "muted": ("#6d6459", "#a89c8c"),
            "line": ("#e3dacb", "#332d25"),
            "accent": ("#d94f2b", "#ff8a5c"),
            "accent_soft": ("#fbeae3", "#3a1d12"),
        },
        "aliases": ["studio", "creative"],
    },
    {
        "key": "portfolio",
        "name": "Portfolio",
        "description": "A personal site: one column, the name set large, and work presented as numbered entries.",
        "audience": "person",
        "layout": "narrow",
        "font_body": 'ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif',
        "font_title": 'ui-serif, "Iowan Old Style", "Palatino Linotype", Palatino, Georgia, serif',
        "measure": "40rem",
        "title_scale": "clamp(2.2rem, 1.5rem + 2.4vw, 3.2rem)",
        "title_weight": "600",
        "title_tracking": "-0.02em",
        "title_transform": "none",
        "radius": "0px",
        "base_size": "17px",
        "header_style": "centered",
        "cards": "framed",
        "tokens": {
            "canvas": ("#ffffff", "#101010"),
            "surface": ("#fafafa", "#191919"),
            "ink": ("#171717", "#ededed"),
            "muted": ("#6a6a6a", "#9d9d9d"),
            "line": ("#e5e5e5", "#2b2b2b"),
            "accent": ("#b45309", "#f0b354"),
            "accent_soft": ("#fdf1e3", "#33240f"),
        },
        "aliases": ["personal", "freelance"],
    },
    {
        "key": "magazine",
        "name": "Magazine",
        "description": "An editorial site: a serif display face, a rule above every section and a standfirst.",
        "audience": "publication",
        "layout": "editorial",
        "font_body": 'ui-serif, "Iowan Old Style", Georgia, "Times New Roman", serif',
        "font_title": 'ui-serif, "Iowan Old Style", Georgia, "Times New Roman", serif',
        "measure": "36rem",
        "title_scale": "clamp(2.4rem, 1.6rem + 3vw, 3.6rem)",
        "title_weight": "700",
        "title_tracking": "-0.03em",
        "title_transform": "none",
        "radius": "0px",
        "base_size": "18px",
        "header_style": "rule",
        "cards": "hairline",
        "tokens": {
            "canvas": ("#fffdf9", "#131110"),
            "surface": ("#f5f0e6", "#1c1917"),
            "ink": ("#1a1714", "#efe9df"),
            "muted": ("#6d6459", "#a1968a"),
            "line": ("#d8cebd", "#332d27"),
            "accent": ("#8a2f2f", "#e08a8a"),
            "accent_soft": ("#f6e7e7", "#371d1d"),
        },
        "aliases": ["editorial", "news", "blog"],
    },
    {
        "key": "commerce",
        "name": "Commerce",
        "description": "A storefront: a compact utility header, product grids and a price set in a tabular figure.",
        "audience": "shop",
        "layout": "shop",
        "font_body": 'ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif',
        "font_title": 'ui-sans-serif, system-ui, "Segoe UI", Roboto, sans-serif',
        "measure": "72rem",
        "title_scale": "clamp(1.9rem, 1.4rem + 1.8vw, 2.6rem)",
        "title_weight": "650",
        "title_tracking": "-0.02em",
        "title_transform": "none",
        "radius": "8px",
        "base_size": "16px",
        "header_style": "utility",
        "cards": "product",
        "tokens": {
            "canvas": ("#ffffff", "#121110"),
            "surface": ("#f7f6f4", "#1d1b19"),
            "ink": ("#1a1a1a", "#efece8"),
            "muted": ("#6b6b6b", "#9d9a95"),
            "line": ("#e5e2dd", "#2e2b28"),
            "accent": ("#146c43", "#6fd39b"),
            "accent_soft": ("#e6f4ec", "#12291d"),
        },
        "aliases": ["restaurant", "shop", "store"],
    },
    {
        "key": "documentation",
        "name": "Documentation",
        "description": "Docs: a sticky table of contents rail, a monospace affordance line and tight line height.",
        "audience": "docs",
        "layout": "sidebar",
        "font_body": 'ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif',
        "font_title": 'ui-sans-serif, system-ui, "Segoe UI", Roboto, sans-serif',
        "measure": "48rem",
        "title_scale": "clamp(1.8rem, 1.4rem + 1.6vw, 2.4rem)",
        "title_weight": "700",
        "title_tracking": "-0.01em",
        "title_transform": "none",
        "radius": "6px",
        "base_size": "16px",
        "header_style": "sticky",
        "cards": "plain",
        "tokens": {
            "canvas": ("#ffffff", "#0e1116"),
            "surface": ("#f6f8fa", "#161b22"),
            "ink": ("#1f2328", "#e6edf3"),
            "muted": ("#636c76", "#8d96a0"),
            "line": ("#d1d9e0", "#262c36"),
            "accent": ("#0969da", "#6cb6ff"),
            "accent_soft": ("#e6f0fb", "#12243a"),
        },
        "aliases": ["docs", "manual"],
    },
    {
        "key": "startup",
        "name": "Startup",
        "description": "A launch site: a centred canvas, an oversized centred headline and a two-step call to action.",
        "audience": "launch",
        "layout": "centered",
        "font_body": 'ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif',
        "font_title": 'ui-sans-serif, system-ui, "Segoe UI", Roboto, sans-serif',
        "measure": "44rem",
        "title_scale": "clamp(2.4rem, 1.3rem + 4.4vw, 4.4rem)",
        "title_weight": "800",
        "title_tracking": "-0.045em",
        "title_transform": "none",
        "radius": "14px",
        "base_size": "17px",
        "header_style": "minimal",
        "cards": "gradient",
        "tokens": {
            "canvas": ("#ffffff", "#0d0b14"),
            "surface": ("#f7f5ff", "#191527"),
            "ink": ("#120f1c", "#ece7fa"),
            "muted": ("#635d75", "#9a92b0"),
            "line": ("#e6e1f5", "#2a2440"),
            "accent": ("#7c3aed", "#c4b0ff"),
            "accent_soft": ("#f1ebff", "#241a45"),
        },
        "aliases": ["launch", "landing"],
    },
    {
        "key": "government",
        "name": "Government",
        "description": "A public-sector portal: a three-part institutional header, high contrast and a visible focus ring.",
        "audience": "public",
        "layout": "institutional",
        "font_body": 'ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif',
        "font_title": 'ui-sans-serif, system-ui, "Segoe UI", Roboto, sans-serif',
        "measure": "44rem",
        "title_scale": "clamp(1.9rem, 1.5rem + 1.6vw, 2.7rem)",
        "title_weight": "700",
        "title_tracking": "-0.01em",
        "title_transform": "none",
        "radius": "2px",
        "base_size": "17px",
        "header_style": "institutional",
        "cards": "listed",
        "tokens": {
            "canvas": ("#ffffff", "#101418"),
            "surface": ("#f2f5f8", "#181d23"),
            "ink": ("#14181d", "#eef1f5"),
            "muted": ("#4d5866", "#9aa5b2"),
            "line": ("#d3dae2", "#2b333c"),
            "accent": ("#003a70", "#79b2f0"),
            "accent_soft": ("#e6eef7", "#132338"),
        },
        "aliases": ["nonprofit", "public", "civic"],
    },
]


# ---------------------------------------------------------------------------------------------
# The generated parts. These are the SAME for all nine: the contract, not the design.
# ---------------------------------------------------------------------------------------------


def manifest_json(theme: dict) -> str:
    tokens = {name: {"light": light, "dark": dark} for name, (light, dark) in theme["tokens"].items()}
    payload = {
        "key": theme["key"],
        "name": theme["name"],
        "version": "1.0.0",
        "description": theme["description"],
        "author": "Omnion",
        "engine": "omnion-web",
        "modes": ["light", "dark"],
        "pageTypes": ["page"],
        "layouts": ["page"],
        "slots": SLOTS,
        "tokens": tokens,
        "settingsSchema": {
            "containerWidth": {"type": "number", "default": 1120, "min": 720, "max": 1600},
            "baseSize": {"type": "number", "default": int(theme["base_size"].rstrip("px")), "min": 14, "max": 22},
            "radius": {"type": "number", "default": int(theme["radius"].rstrip("px")), "min": 0, "max": 28},
        },
        "compatibility": {"engine": "omnion-web", "minVersion": "0.1.0"},
        "previewImage": "preview.svg",
        "screenshots": [],
        "aliases": theme["aliases"],
    }
    return json.dumps(payload, indent=2) + "\n"


def package_json(theme: dict) -> str:
    payload = {
        "name": f"@omnion/theme-{theme['key']}",
        "version": "1.0.0",
        "private": True,
        "description": f"Omnion's {theme['name']} theme — {theme['description']}",
        "type": "module",
        "exports": {
            ".": "./src/index.ts",
            "./styles.css": f"./styles/{theme['key']}.css",
            "./omnion.theme.json": "./omnion.theme.json",
        },
        "dependencies": {
            "@omnion/theme-sdk": "workspace:*",
            "@omnion/types": "workspace:*",
        },
        "peerDependencies": {"react": ">=19"},
        "devDependencies": {"@types/react": "19.3.0"},
    }
    return json.dumps(payload, indent=2) + "\n"


def block_renderer_ts(theme: dict) -> str:
    p = class_prefix(theme["key"])
    return f'''/**
 * The {theme['name']} theme's block renderer.
 *
 * The platform owns what a block MEANS (a FAQ is a description list, a testimonial is a
 * figure with a caption), so the switch statement lives in the SDK. What a theme owns is the
 * class prefix its own stylesheet rules match, and the few shapes where presentation is the
 * block's whole point — see `page-layout.tsx` for the {theme['name']} wrappers.
 */
import {{ createBlockRenderer }} from "@omnion/theme-sdk";

/** The block renderer this theme draws content with. */
export const {{ Block, BlockTree, bodyParagraphs, prefix }} = createBlockRenderer({{
  prefix: "{p}",
}});
'''


def theme_ts(theme: dict) -> str:
    return f'''/**
 * The {theme['name']} theme.
 *
 * Theme code is presentation: it reads the content the renderer hands it and nothing else.
 * The manifest is the single source of the metadata — the version and modes below are read
 * from the file, not restated, so the gallery and the renderer can never disagree.
 */
import {{ defineTheme }} from "@omnion/theme-sdk";

import manifest from "../omnion.theme.json";
import {{ {theme['key']}PageLayout }} from "./page-layout";

/** The {theme['name']} theme, as the renderer activates it. */
export const {theme['key']}Theme = defineTheme({{
  key: manifest.key,
  name: manifest.name,
  version: manifest.version,
  modes: manifest.modes,
  pageTypes: manifest.pageTypes,
  PageLayout: {theme['key']}PageLayout,
}});
'''


def index_ts(theme: dict) -> str:
    return f'''/** Public surface of the {theme['name']} theme. */
export {{ Block, BlockTree }} from "./block-renderer";
export {{ {theme['key']}PageLayout }} from "./page-layout";
export {{ {theme['key']}Theme }} from "./theme";
export {{ default as manifest }} from "../omnion.theme.json";
'''


def write(path: pathlib.Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    if not content.strip():
        raise SystemExit(f"REFUSING: {path} would be empty")
    if content.endswith("\x00"):
        raise SystemExit(f"REFUSING: {path} contains a NUL")


def main() -> int:
    for theme in THEME_DATA:
        key = theme["key"]
        root = THEMES / key
        if root.exists():
            shutil.rmtree(root)
        write(root / "omnion.theme.json", manifest_json(theme))
        write(root / "package.json", package_json(theme))
        write(root / "src" / "index.ts", index_ts(theme))
        write(root / "src" / "theme.ts", theme_ts(theme))
        write(root / "src" / "block-renderer.ts", block_renderer_ts(theme))
        write(root / "src" / "page-layout.tsx", layout_tsx(theme))
        write(root / "styles" / f"{key}.css", stylesheet(theme))
        write(root / "preview.svg", preview_svg(theme))
        layout_bytes = (root / "src" / "page-layout.tsx").stat().st_size
        css_bytes = (root / "styles" / f"{key}.css").stat().st_size
        print(f"  {key:14} layout {layout_bytes:6d} B   css {css_bytes:6d} B")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

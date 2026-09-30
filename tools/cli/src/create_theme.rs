//! `omnion create-theme <key>` — scaffold a theme package (REQ-062, slice 4).
//!
//! A theme is presentation only, and the contract it has to satisfy is small and fixed
//! (docs/03-FRONTONT.md, `packages/theme-sdk`): a manifest, a `defineTheme` wiring file, a
//! page layout and a stylesheet. Writing those four by hand for a first theme means getting
//! four import paths and a class-name prefix right before anything renders, so the command
//! emits the skeleton from the same shapes `themes/minimal` ships — the one theme the
//! renderer already resolves.
//!
//! ```text
//! omnion create-theme editorial                 # → themes/editorial
//! omnion create-theme editorial --dir packages  # → packages/editorial
//! omnion create-theme editorial --force         # overwrite an existing directory
//! ```
//!
//! Two properties matter more than the convenience:
//!
//!   * **It never overwrites by accident.** A scaffolder that silently replaces a working
//!     theme is a data-loss bug wearing a developer-tool costume, so an existing directory is
//!     refused unless `--force` is given, and even then a non-empty directory is refused.
//!   * **The key is validated before anything is written.** The key reaches a directory name, a
//!     package name and a `class` prefix, so `../evil` and `My Theme` have to be refused at the
//!     command line rather than becoming a file nobody can delete.

use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;

use crate::args::CreateThemeOptions;

/// Report the outcome of a scaffold as an exit code.
///
/// # Errors
///
/// Nothing here returns an error: the run has already been performed by [`run`], and this
/// only turns its verdict into the exit code the shell reads. A scaffolder that exits 0 on a
/// refusal is a scaffolder a script cannot trust.
pub fn main(options: CreateThemeOptions) -> ExitCode {
    let Some(key) = options.key.clone() else {
        eprintln!("omnion create-theme: a theme key is required");
        eprintln!();
        eprintln!("    omnion create-theme <key> [--dir <path>] [--force]");
        return ExitCode::from(2);
    };

    match run(
        &key,
        options
            .dir
            .as_deref()
            .map(Path::new)
            .unwrap_or(default_dir()),
        options.force,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(ScaffoldError::Usage(message)) => {
            eprintln!("omnion create-theme: {message}");
            ExitCode::from(2)
        }
        Err(ScaffoldError::Failed(message)) => {
            eprintln!("omnion create-theme: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Where a theme lands when `--dir` is not given: `themes/`, the directory the workspace
/// glob already picks up, so the scaffolding lands somewhere it is already part of the build
/// rather than in a directory the author has to wire up afterwards.
fn default_dir() -> &'static Path {
    Path::new("themes")
}

/// Files the command writes, in the order the closing report lists them.
const FILES: [(&str, &str); 6] = [
    ("omnion.theme.json", "manifest"),
    ("package.json", "workspace package"),
    ("src/index.ts", "public surface"),
    ("src/theme.ts", "`defineTheme` wiring"),
    ("src/page-layout.tsx", "page renderer"),
    ("styles/<key>.css", "canvas, type scale, light/dark"),
];

/// How a run ended.
///
/// `Usage` exits 2 and `Failed` exits 1, which is the difference the shell needs: a typo in
/// the command line and a filesystem that said no are not the same failure, and a script
/// that cannot tell them apart retries the wrong one.
#[derive(Debug)]
enum ScaffoldError {
    /// The command line cannot produce a usable theme.
    Usage(String),
    /// The filesystem refused the work.
    Failed(String),
}

impl From<String> for ScaffoldError {
    fn from(message: String) -> Self {
        Self::Usage(message)
    }
}

/// Scaffold the theme and report what it wrote.
///
/// # Errors
///
/// Returns a usage error for a key that is not a key, a path that already holds a theme
/// without `--force`, or a directory that cannot be created; a failure error when a file
/// cannot be written or a created tree cannot be cleaned up after a failure.
pub fn run(key: &str, dir: &Path, force: bool) -> Result<(), ScaffoldError> {
    let key = validate_key(key)?;
    let root = dir.join(&key);

    if root.exists() {
        if !force {
            return Err(ScaffoldError::Usage(format!(
                "{} already exists — pass --force to scaffold into it, or choose another key",
                root.display()
            )));
        }
        if !root.is_dir() {
            return Err(ScaffoldError::Usage(format!(
                "{} exists and is not a directory",
                root.display()
            )));
        }
        // `--force` means "I know what is in there", not "delete whatever this is". A
        // directory holding anything at all is somebody's work.
        if let Some(first) = first_entry(&root) {
            return Err(ScaffoldError::Usage(format!(
                "{} is not empty ({}); --force will not delete it",
                root.display(),
                first
            )));
        }
    }

    let name = display_name(&key);
    let files = skeleton(&key, &name);

    for (relative, contents) in &files {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                ScaffoldError::Failed(format!("could not create {}: {err}", parent.display()))
            })?;
        }
        std::fs::write(&path, contents).map_err(|err| {
            // Half a theme is worse than none: a manifest with no stylesheet loads in the
            // gallery and renders nothing, which reads as a platform bug rather than as a
            // scaffolder that died. Take the tree back down before returning.
            let _ = std::fs::remove_dir_all(&root);
            ScaffoldError::Failed(format!("could not write {}: {err}", path.display()))
        })?;
    }

    println!("Scaffolded theme {key} in {}", root.display());
    println!();
    for (relative, role) in FILES {
        let name = relative.replace("<key>", &key);
        println!("  {name}");
        println!("        {role}");
    }
    println!();
    println!("Next:");
    println!("  1. fill in the tokens and slots in omnion.theme.json");
    println!("  2. pnpm --filter @omnion/web add @omnion/theme-{key}@workspace:*");
    println!("     (the renderer imports each theme by its package name, so a theme nobody");
    println!("      depends on is never linked into it — a bare `pnpm install` leaves the");
    println!("      registry import failing with TS2307)");
    println!("  3. register it in apps/web/lib/theme.ts — the renderer resolves themes by key");
    Ok(())
}

/// Whether a key can be a directory, a package name and a CSS prefix.
///
/// The shape is the one the theme table's key column and the package manager both already
/// accept (lowercase, digits, single dashes), so a key the scaffolder accepts is a key the
/// platform can install, upload and activate without a second validation story.
fn validate_key(key: &str) -> Result<String, ScaffoldError> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err(ScaffoldError::Usage("a theme key is required".to_owned()));
    }
    if trimmed.len() > 48 {
        return Err(ScaffoldError::Usage(format!(
            "'{trimmed}' is {} characters; a theme key is at most 48",
            trimmed.len()
        )));
    }
    if trimmed.starts_with('-') || trimmed.ends_with('-') {
        return Err(ScaffoldError::Usage(format!(
            "'{trimmed}' must not start or end with a dash"
        )));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(ScaffoldError::Usage(format!(
            "'{trimmed}' is not a theme key: use lowercase letters, digits and single dashes (like 'editorial' or 'nonprofit')"
        )));
    }
    Ok(trimmed.to_owned())
}

/// `editorial` → `Editorial`, so the scaffolded manifest is not titled "editorial".
fn display_name(key: &str) -> String {
    let mut name = String::with_capacity(key.len());
    let mut capitalise = true;
    for c in key.chars() {
        if c == '-' {
            capitalise = true;
            continue;
        }
        name.push(if capitalise {
            c.to_ascii_uppercase()
        } else {
            c
        });
        capitalise = false;
    }
    name
}

/// The first entry of a directory, or `None` when it is empty.
fn first_entry(dir: &Path) -> Option<String> {
    let mut entries: Vec<String> = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    entries.into_iter().next()
}

/// The files to write, as `(relative path, contents)`.
fn skeleton(key: &str, name: &str) -> Vec<(String, String)> {
    vec![
        ("omnion.theme.json".to_owned(), manifest(key, name)),
        ("package.json".to_owned(), package_json(key, name)),
        ("src/index.ts".to_owned(), index_ts(key)),
        ("src/theme.ts".to_owned(), theme_ts(key, name)),
        ("src/page-layout.tsx".to_owned(), page_layout(key, name)),
        (format!("styles/{key}.css"), stylesheet(key)),
    ]
}

/// The v2 manifest. Every optional field the contract knows is present, because a first
/// author should see the shape of a complete manifest rather than discover the fields from
/// another theme's file.
fn manifest(key: &str, name: &str) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "{{\n  \"key\": \"{key}\",\n  \"name\": \"{name}\",\n  \"version\": \"0.1.0\",\n  \
         \"description\": \"TODO: one line about what {name} is for.\",\n  \
         \"author\": \"TODO: who ships this theme\",\n  \"engine\": \"omnion-web\",\n  \
         \"modes\": [\"light\", \"dark\"],\n  \
         \"pageTypes\": [\"page\"],\n  \
         \"layouts\": [\"header\", \"footer\", \"page\"],\n  \
         \"slots\": [\"header\", \"footer\", \"page\"],\n  \
         \"tokens\": {{\n    \"bg\":      {{ \"light\": \"#ffffff\", \"dark\": \"#111111\" }},\n    \
         \"fg\":      {{ \"light\": \"#111111\", \"dark\": \"#f5f5f5\" }},\n    \
         \"muted\":   {{ \"light\": \"#57534e\", \"dark\": \"#a8a29e\" }},\n    \
         \"accent\":  {{ \"light\": \"#1d4ed8\", \"dark\": \"#93c5fd\" }},\n    \
         \"border\":  {{ \"light\": \"#e7e5e4\", \"dark\": \"#292524\" }}\n  }},\n  \
         \"settingsSchema\": {{\n    \"baseSize\":  {{ \"type\": \"number\", \"default\": 17, \"min\": 14, \"max\": 22 }},\n    \
         \"radius\":    {{ \"type\": \"number\", \"default\": 8, \"min\": 0, \"max\": 32 }},\n    \
         \"container\": {{ \"type\": \"number\", \"default\": 1120, \"min\": 640, \"max\": 1600 }}\n  }},\n  \
         \"compatibility\": {{ \"engine\": \"omnion-web\", \"minVersion\": \"0.1.0\" }},\n  \
         \"previewImage\": \"styles/preview.png\",\n  \
         \"screenshots\": [],\n  \
         \"aliases\": []\n}}"
    )
    .expect("a manifest literal always writes");
    out
}

/// The workspace package. `workspace:*` on the two packages the contract imports, and the
/// stylesheet and manifest as named exports so a bundler never has to guess a path.
fn package_json(key: &str, name: &str) -> String {
    format!(
        "{{\n  \"name\": \"@omnion/theme-{key}\",\n  \"version\": \"0.1.0\",\n  \"private\": true,\n  \
         \"description\": \"Omnion's {name} theme.\",\n  \"type\": \"module\",\n  \
         \"exports\": {{\n    \".\": \"./src/index.ts\",\n    \"./styles.css\": \
         \"./styles/{key}.css\",\n    \"./omnion.theme.json\": \
         \"./omnion.theme.json\"\n  }},\n  \"dependencies\": {{\n    \
         \"@omnion/theme-sdk\": \"workspace:*\",\n    \
         \"@omnion/types\": \"workspace:*\"\n  }},\n  \
         \"peerDependencies\": {{\n    \"react\": \">=19\"\n  }},\n  \
         \"devDependencies\": {{\n    \"@types/react\": \"19.3.0\"\n  }}\n}}\n"
    )
}

/// The public surface, so the renderer imports one symbol.
///
/// A function rather than a const because the export names the layout component, and that
/// name carries the theme's class prefix: a const would need a placeholder that a reader
/// (and a test) would have to notice was still a placeholder. The first version of this file
/// shipped exactly that way — `{PLACEHOLDER}PageLayout` — and every unit test still passed,
/// because "the file is not empty" is not "the file is valid TypeScript".
fn index_ts(key: &str) -> String {
    let symbol = layout_component(key);
    format!(
        r#"export {{ theme }} from "./theme";
export {{ {symbol} }} from "./page-layout";
export {{ default as manifest }} from "../omnion.theme.json";
"#
    )
}

/// The `defineTheme` wiring. The manifest stays the single source of the metadata, so a
/// version bump is a manifest edit rather than a code edit.
fn theme_ts(key: &str, name: &str) -> String {
    let component = layout_component(key);
    format!(
        r#"/**
 * The {name} theme.
 *
 * Theme code is presentation: it reads the content the renderer hands it and nothing else.
 * The manifest is the single source of the metadata — the version and modes below are read
 * from the file, not restated, so the gallery and the renderer can never disagree.
 */
import {{ defineTheme }} from "@omnion/theme-sdk";

import manifest from "../omnion.theme.json";
import {{ {component} }} from "./page-layout";

/** The {name} theme, as the renderer activates it. */
export const theme = defineTheme({{
  key: manifest.key,
  name: manifest.name,
  version: manifest.version,
  modes: manifest.modes,
  pageTypes: manifest.pageTypes,
  PageLayout: {component},
}});
"#
    )
}

/// The page renderer. It reads the published page and the block tree when the revision has
/// one, and falls back to the plain body otherwise — the same fallback `themes/minimal` has,
/// so a page published before the block system exists still renders.
fn page_layout(key: &str, name: &str) -> String {
    let class = class_prefix(key);
    let component = layout_component(key);
    format!(
        r#"/**
 * The {name} theme's page layout.
 *
 * A revision that carries a block tree is drawn from it; a revision without one falls back
 * to the plain body, where blank lines separate paragraphs. That fallback is what lets a
 * page published before the block system existed still render.
 *
 * TODO: this is the one file every theme has to be different in — a header, a hero, a card
 * system and a type scale of its own. Swap the markup below and the tokens in the
 * stylesheet; nothing else in the platform has to change.
 */
import type {{ PageLayoutProps }} from "@omnion/theme-sdk";
import type {{ ContentBlock }} from "@omnion/types";

/** Split a plain-text body into paragraphs on blank lines. */
export function bodyParagraphs(body: string): string[] {{
  return body
    .split(/\n\s*\n/)
    .map((block) => block.trim())
    .filter((block) => block.length > 0);
}}

/** Render one published page. */
export function {component}({{ content }}: PageLayoutProps) {{
  const {{ site, page, revision }} = content;
  const blocks = Array.isArray(revision.blocks)
    ? (revision.blocks as ContentBlock[])
    : [];
  const paragraphs = bodyParagraphs(revision.body);

  return (
    <div className="{class}-shell">
      <header className="{class}-header">
        <a className="{class}-brand" href="/">
          {{site.name}}
        </a>
      </header>

      <main className="{class}-main">
        <article className="{class}-article">
          <h1 className="{class}-title">{{revision.title}}</h1>
          {{revision.summary ? <p className="{class}-summary">{{revision.summary}}</p> : null}}
          {{page.page_type}}
          <div className="{class}-body">
            {{blocks.length > 0
              ? blocks.map((block, index) => <section key={{index}}>TODO: render {{block.type}}</section>)
              : paragraphs.map((paragraph, index) => <p key={{index}}>{{paragraph}}</p>)}}
          </div>
        </article>
      </main>

      <footer className="{class}-footer">
        <span>{{site.name}}</span>
        <span>Powered by Omnion</span>
      </footer>
    </div>
  );
}}
"#
    )
}

/// The stylesheet: the tokens as CSS custom properties, then a canvas and a type scale that
/// reads them. Dark mode is a token swap, never a second set of rules.
fn stylesheet(key: &str) -> String {
    let class = class_prefix(key);
    let block = |selector: &str, body: &str| format!("\n{selector} {{\n{body}\n}}\n");
    let mut css = String::new();
    css.push_str(&format!(
        r#"/**
 * The {key} theme.
 *
 * The tokens are the theme: `--fg`/`--bg` are emitted from the site settings revision, and
 * this file is what consumes them. A theme that needs a real difference from another theme
 * gets it from the type scale, the measure and the card system below — not from swapping
 * six colours.
 */

:root {{
  --fg: #111111;
  --bg: #ffffff;
  --muted: #57534e;
  --accent: #1d4ed8;
  --border: #e7e5e4;
  --measure: 68ch;
  --radius: 8px;
  --base-size: 17px;
}}

@media (prefers-color-scheme: dark) {{
  :root {{
    --fg: #f5f5f5;
    --bg: #111111;
    --muted: #a8a29e;
    --accent: #93c5fd;
    --border: #292524;
  }}
}}
"#
    ));

    css.push_str(&block(
        "*, *::before, *::after",
        "  box-sizing: border-box;",
    ));
    css.push_str(&block(
        "body",
        "  margin: 0;\n  background: var(--bg);\n  color: var(--fg);\n  \
         font-family: ui-sans-serif, system-ui, -apple-system, \"Segoe UI\", sans-serif;\n  \
         font-size: var(--base-size);\n  line-height: 1.6;\n  \
         -webkit-font-smoothing: antialiased;",
    ));
    css.push_str(&block(
        &format!(".{class}-shell"),
        "  min-height: 100dvh;\n  display: flex;\n  flex-direction: column;",
    ));
    css.push_str(&block(
        &format!(".{class}-header"),
        "  display: flex;\n  align-items: center;\n  justify-content: space-between;\n  \
         gap: 1rem;\n  padding: 1.25rem 1.5rem;\n  border-bottom: 1px solid var(--border);",
    ));
    css.push_str(&block(
        &format!(".{class}-brand"),
        "  color: var(--fg);\n  font-weight: 600;\n  text-decoration: none;",
    ));
    css.push_str(&block(
        &format!(".{class}-main"),
        "  flex: 1;\n  width: 100%;\n  max-width: 1120px;\n  margin-inline: auto;\n  \
         padding: 3rem 1.5rem 4rem;",
    ));
    css.push_str(&block(
        &format!(".{class}-article"),
        "  max-width: var(--measure);",
    ));
    css.push_str(&block(
        &format!(".{class}-title"),
        "  margin: 0 0 0.5rem;\n  font-size: clamp(2rem, 1.4rem + 2.4vw, 3.25rem);\n  \
         line-height: 1.1;\n  letter-spacing: -0.02em;\n  font-weight: 700;",
    ));
    css.push_str(&block(
        &format!(".{class}-summary"),
        "  margin: 0 0 1rem;\n  color: var(--muted);\n  font-size: 1.125em;",
    ));
    css.push_str(&block(&format!(".{class}-body"), "  white-space: normal;"));
    css.push_str(&block(
        &format!(".{class}-footer"),
        "  display: flex;\n  justify-content: space-between;\n  gap: 1rem;\n  \
         padding: 1.5rem;\n  border-top: 1px solid var(--border);\n  color: var(--muted);\n  \
         font-size: 0.875em;",
    ));
    css.push_str(&format!(
        "\n/* Keyboard focus must stay visible in every mode — a theme that removes the ring\n   \
         is a theme that fails WCAG 2.4.7, whatever its colours look like. */\n\
         :where({class}-shell) :focus-visible {{\n  outline: 2px solid var(--accent);\n  \
         outline-offset: 2px;\n}}\n"
    ));
    css
}

/// The CSS class prefix: the key with its dashes kept (`non-profit` → `np-non-profit`), so
/// two themes in one bundle can never collide on a class name.
fn class_prefix(key: &str) -> String {
    format!("{}-{key}", prefix_initials(key).to_lowercase())
}

/// The component's identifier body: the key in PascalCase with its dashes removed
/// (`non-profit` → `NonProfit`).
///
/// Two prefixes, not one, and the reason is that they go to two different languages. A CSS
/// class may contain dashes; a TypeScript identifier may not. The first version reused the
/// class prefix for the component name, which produced `ed-editorialPageLayout` in the
/// wiring file and the surface file — invalid TypeScript in two of the six generated files,
/// from a scaffolder whose every test passed.
fn component_prefix(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut capitalise = true;
    for c in key.chars() {
        if c == '-' {
            capitalise = true;
            continue;
        }
        out.push(if capitalise {
            c.to_ascii_uppercase()
        } else {
            c
        });
        capitalise = false;
    }
    out
}

/// The two letters that open a theme's prefix: the first letter of the key, then the first
/// letter of the *next* word.
///
/// The naive "first two letters" gives `non-profit` → `no`, which collides with a theme
/// literally called `nonprofit` — the two would share a prefix in a bundle, and a
/// stylesheet debugging session would start with "why is my theme's CSS applying to the
/// other one". Skipping the dash is one `.filter()` and removes the collision class.
fn prefix_initials(key: &str) -> String {
    let mut initials = String::with_capacity(2);
    let mut previous_was_dash = true;
    for c in key.chars() {
        if c == '-' {
            previous_was_dash = true;
            continue;
        }
        if !c.is_ascii_alphabetic() {
            continue;
        }
        if previous_was_dash && initials.len() < 2 {
            initials.push(c.to_ascii_uppercase());
        }
        previous_was_dash = false;
    }
    // A single-word key still needs two letters when it has them, so `docs` → `DO` and not
    // `D` (a one-letter prefix collides with every other one-letter key).
    if initials.len() < 2 {
        let extra: Vec<char> = key.chars().filter(char::is_ascii_alphabetic).collect();
        for c in extra {
            if initials.len() >= 2 {
                break;
            }
            if !initials.contains(c.to_ascii_uppercase()) {
                initials.push(c.to_ascii_uppercase());
            }
        }
    }
    initials
}

/// The layout component's name for a key: `editorial` → `edEditorialPageLayout`.
///
/// The initials are lower-cased, because the component continues into the key itself:
/// upper-casing them gives `EDeditorialPageLayout`, which is both ugly and a component that
/// starts with a run of capitals. The CSS prefix lower-cases them anyway, so the two
/// functions differ only in case at the front and in the word boundaries after it.
fn layout_component(key: &str) -> String {
    let initials: String = prefix_initials(key).to_lowercase().chars().collect();
    format!("{initials}{}PageLayout", component_prefix(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_key_is_accepted() {
        assert_eq!(validate_key("editorial").expect("accepts"), "editorial");
        assert_eq!(validate_key(" non-profit ").expect("trims"), "non-profit");
    }

    #[test]
    fn a_key_that_is_not_a_key_is_refused_with_the_reason() {
        for bad in [
            "", "   ", "My Theme", "../evil", "my_theme", "-lead", "trail-",
        ] {
            assert!(
                validate_key(bad).is_err(),
                "'{bad}' must be refused as a theme key"
            );
        }
        assert!(validate_key("Editorial").is_err());
        assert!(validate_key("a/b").is_err());
        assert!(validate_key(&"x".repeat(49)).is_err());
        assert!(validate_key(&"x".repeat(48)).is_ok());
    }

    #[test]
    fn the_display_name_is_the_key_title_cased() {
        assert_eq!(display_name("editorial"), "Editorial");
        assert_eq!(display_name("non-profit"), "NonProfit");
        assert_eq!(display_name("magazine"), "Magazine");
    }

    #[test]
    fn the_class_prefix_carries_the_key_so_two_themes_cannot_collide() {
        assert_eq!(class_prefix("editorial"), "ed-editorial");
        assert_eq!(class_prefix("nonprofit"), "no-nonprofit");
        assert_ne!(class_prefix("news"), class_prefix("newsletter"));
    }

    /// `non-profit` and `nonprofit` are different keys and must not share a prefix: two
    /// themes whose CSS classes start the same way is a debugging session nobody enjoys.
    #[test]
    fn a_dashed_key_does_not_collide_with_its_undashed_twin() {
        assert_eq!(class_prefix("non-profit"), "np-non-profit");
        assert_eq!(class_prefix("nonprofit"), "no-nonprofit");
        assert_ne!(class_prefix("non-profit"), class_prefix("nonprofit"));
        assert_ne!(
            layout_component("non-profit"),
            layout_component("nonprofit")
        );
        assert_eq!(layout_component("non-profit"), "npNonProfitPageLayout");
    }

    /// A CSS class may contain a dash; a TypeScript identifier may not. The two prefixes are
    /// therefore different functions, and the second version of this scaffolder used one for
    /// both — which put `ed-editorialPageLayout` into the wiring file and the surface file.
    /// The scaffolder's whole test suite was green: the manifest was valid JSON, the file
    /// was not empty and the symbol *string* matched across the files. Only the language
    /// objected. So the test is about the language's rule, not about matching strings.
    #[test]
    fn a_component_name_is_a_valid_typescript_identifier() {
        for key in ["editorial", "non-profit", "magazine", "docs", "a-b-c"] {
            let name = layout_component(key);
            let mut chars = name.chars();
            let first = chars.next().expect("a component name is not empty");
            assert!(
                first.is_ascii_alphabetic() || first == '_' || first == '$',
                "'{name}' must not start with '{first}'"
            );
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$'),
                "'{name}' is not a valid TypeScript identifier (a CSS class prefix is not one)"
            );
        }
        assert_eq!(layout_component("editorial"), "edEditorialPageLayout");
        // The CSS prefix keeps its dashes, and the two are genuinely different strings.
        assert_eq!(class_prefix("non-profit"), "np-non-profit");
        assert_ne!(class_prefix("non-profit"), component_prefix("non-profit"));
    }

    /// The scaffolded manifest must satisfy the same `manifest_shape` the platform's loader
    /// runs, because a scaffold that emits a manifest the platform refuses is a scaffolder
    /// that reports success and produces a theme that cannot be activated.
    #[test]
    fn the_scaffolded_manifest_passes_the_platform_validator() {
        let key = "editorial";
        let text = manifest(key, "Editorial");
        let parsed: serde_json::Value =
            serde_json::from_str(&text).expect("the manifest must be valid JSON");

        let shape = omnion_content::themes::manifest_shape(&parsed)
            .expect("the scaffolded manifest must satisfy the v2 contract");
        assert_eq!(shape.slots, 3);
        assert_eq!(shape.tokens, 5);
        assert_eq!(shape.modes, vec!["light", "dark"]);
        for field in [
            "settingsSchema",
            "compatibility",
            "previewImage",
            "screenshots",
            "aliases",
        ] {
            assert!(
                shape.extras.contains(&field),
                "the scaffold must show the '{field}' field"
            );
        }
    }

    #[test]
    fn every_skeleton_file_is_valid_json_where_it_claims_to_be() {
        let files = skeleton("editorial", "Editorial");
        for (path, contents) in &files {
            if path.ends_with(".json") {
                serde_json::from_str::<serde_json::Value>(contents)
                    .unwrap_or_else(|err| panic!("{path} is not valid JSON: {err}"));
            }
            assert!(!contents.is_empty(), "{path} must not be empty");
        }
        let paths: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "omnion.theme.json",
                "package.json",
                "src/index.ts",
                "src/theme.ts",
                "src/page-layout.tsx",
                "styles/editorial.css",
            ]
        );
    }

    /// Every symbol the wiring file imports must be exported by the file it imports, or the
    /// scaffolded theme does not typecheck — and a scaffolder whose output does not compile
    /// is a scaffolder that hands back an afternoon.
    #[test]
    fn the_wiring_imports_the_layout_the_page_layout_exports() {
        let key = "editorial";
        let wiring = theme_ts(key, "Editorial");
        let layout = page_layout(key, "Editorial");

        let symbol = layout_component(key);
        assert!(
            wiring.contains(&format!("import {{ {symbol} }} from \"./page-layout\";")),
            "theme.ts must import {symbol} from the page layout"
        );
        assert!(
            layout.contains(&format!("export function {symbol}(")),
            "page-layout.tsx must export {symbol}"
        );
    }

    /// The whole file set, checked for one thing: an unexpanded template placeholder.
    ///
    /// The scaffolder's own first version shipped `{PLACEHOLDER}PageLayout` in
    /// `src/index.ts`, and every other test passed — the manifest was valid JSON, the
    /// symbols the *wiring* file imported existed, and no file was empty. What made it a
    /// defect is that the *surface* file re-exported a name nobody had defined, which only
    /// a typecheck or a reader would ever notice. A test that walks the generated files for
    /// a leftover placeholder turns a silent afternoon-losing bug into a red line, and it
    /// costs one `contains`.
    #[test]
    fn no_generated_file_carries_an_unexpanded_placeholder() {
        for key in ["editorial", "non-profit", "magazine"] {
            for (path, contents) in skeleton(key, &display_name(key)) {
                assert!(
                    !contents.contains("{PLACEHOLDER}"),
                    "{path} still carries a template placeholder"
                );
                // A doubled brace pair is what a Rust `format!` literal leaves behind when a
                // template was pasted in without escaping it; the same class of bug.
                assert!(
                    !contents.contains("{{") || path.ends_with(".css"),
                    "{path} still carries unescaped braces from its template"
                );
            }
        }
    }

    /// Every name the surface re-exports must be a name some file in the set actually
    /// defines. A surface that exports a symbol nobody defines is the exact defect the
    /// placeholder test found, and this checks the class rather than the instance.
    #[test]
    fn every_symbol_the_surface_reexports_is_defined_somewhere_in_the_set() {
        let key = "editorial";
        let files = skeleton(key, "Editorial");
        let surface = files
            .iter()
            .find(|(path, _)| path == "src/index.ts")
            .map(|(_, contents)| contents.as_str())
            .expect("the surface file is scaffolded");

        for line in surface.lines().filter(|line| line.starts_with("export {")) {
            let name = line
                .split('{')
                .nth(1)
                .and_then(|rest| rest.split('}').next())
                .expect("an export line names a symbol")
                .trim();
            let defined = files.iter().any(|(_, contents)| {
                contents.contains(&format!("export function {name}("))
                    || contents.contains(&format!("export const {name} "))
                    || contents.contains(&format!("export const {name}:"))
                    || contents.contains(&format!("export {{ {name} }}"))
            });
            assert!(
                defined,
                "the surface re-exports '{name}' and nothing defines it"
            );
        }
    }

    #[test]
    fn the_stylesheet_reads_the_tokens_rather_than_hard_coding_a_palette() {
        let css = stylesheet("editorial");
        for token in ["--fg", "--bg", "--muted", "--accent", "--border"] {
            assert!(
                css.contains(&format!("var({token})")),
                "the stylesheet must consume {token}"
            );
        }
        assert!(
            css.contains("prefers-color-scheme: dark"),
            "dark mode must be a token swap, not a second set of rules"
        );
        assert!(
            css.contains(":focus-visible"),
            "a theme must not ship without a visible focus ring"
        );
    }
}

/**
 * The command centre's own logic (docs/requests/REQ-032, slice 1) — kept out of the component so
 * the prefix model and the ranking can be read (and changed) without touching the dialog.
 *
 * The API owns *which* commands exist and who may run them; this module owns what the box does
 * with what it is given: reading a leading `>` `@` `#` `:` `?` as a mode, ranking commands against
 * the typed text, and turning a stored recent into a row.
 */
import type { CommandInfo, CommandRecent } from "./api";

/** The narrowing the box is in. */
export type PaletteMode = "all" | "commands" | "people" | "sites" | "settings" | "help";

/** The prefix characters, in the order the footer lists them. */
export const PREFIXES: readonly {
  prefix: string;
  mode: PaletteMode;
  label: string;
  placeholder: string;
}[] = [
  { prefix: ">", mode: "commands", label: "Commands", placeholder: "Type a command…" },
  { prefix: "@", mode: "people", label: "People", placeholder: "Search people…" },
  { prefix: "#", mode: "sites", label: "Sites", placeholder: "Search sites…" },
  { prefix: ":", mode: "settings", label: "Settings", placeholder: "Search settings…" },
  { prefix: "?", mode: "help", label: "Shortcuts", placeholder: "Keyboard shortcuts" },
];

/** What the box current holds: the mode its prefix selects, and the text after it. */
export type ModeReading = {
  mode: PaletteMode;
  /** The text after the prefix — what is actually searched for. */
  term: string;
  /** The prefix character that put the box in this mode; `""` when there is none. */
  prefix: string;
};

/**
 * Read the box's content as a mode plus a term.
 *
 * Only a leading character selects a mode (a `#` in the middle of a sentence stays part of the
 * query), and `?` selects the shortcut sheet only when it stands alone — otherwise it is a
 * question someone is searching for.
 */
export function readMode(input: string): ModeReading {
  const leading = input.replace(/^\s+/, "");
  if (leading === "?") {
    return { mode: "help", term: "", prefix: "?" };
  }
  const entry = PREFIXES.find((candidate) => leading.startsWith(candidate.prefix));
  if (!entry) {
    return { mode: "all", term: input.trim(), prefix: "" };
  }
  return {
    mode: entry.mode,
    term: leading.slice(entry.prefix.length).trim(),
    prefix: entry.prefix,
  };
}

/** The chip's label: the mode's own name. */
export function modeLabel(mode: PaletteMode): string {
  return PREFIXES.find((entry) => entry.mode === mode)?.label ?? "Search";
}

/** The input's placeholder in one mode. */
export function modePlaceholder(mode: PaletteMode): string {
  return PREFIXES.find((entry) => entry.mode === mode)?.placeholder ?? "Search Omnion…";
}

/**
 * The provider a narrowed mode searches.
 *
 * `all` searches everything the caller may read; the others name the one provider the mode is
 * about, so `#acme` is the sites answer rather than a general one.
 */
export function modeTypes(mode: PaletteMode): string | null {
  switch (mode) {
    case "people":
      return "users";
    case "sites":
      return "sites";
    case "settings":
      return "settings";
    default:
      return null;
  }
}

/** `true` when the mode shows search results. */
export function modeSearches(mode: PaletteMode): boolean {
  return mode !== "help";
}

/**
 * How well one command answers `term`: lower is better, `-1` means it does not match.
 *
 * The order is the one a person expects — an exact title first, then prefixes, then the words
 * inside it, then keywords and aliases, and the id last (it is a handle, not a label). A
 * multi-word term matches when every word appears somewhere in the command's own text, so
 * "create page" finds "Create a page".
 */
export function rankCommand(command: CommandInfo, term: string): number {
  const needle = term.trim().toLowerCase();
  if (!needle) {
    return 0;
  }

  const title = command.title.toLowerCase();
  const aliases = [...command.aliases, ...command.keywords].map((value) => value.toLowerCase());
  const haystack = [title, command.id.toLowerCase(), command.hint.toLowerCase(), ...aliases].join(
    " ",
  );

  if (title === needle) return 0;
  if (title.startsWith(needle)) return 1;
  if (title.includes(needle)) return 2;
  if (aliases.includes(needle)) return 3;
  if (aliases.some((value) => value.startsWith(needle))) return 4;
  if (aliases.some((value) => value.includes(needle))) return 5;

  const words = needle.split(/\s+/).filter(Boolean);
  if (words.length > 1 && words.every((word) => haystack.includes(word))) {
    return 6;
  }

  if (command.id.toLowerCase().includes(needle)) return 7;
  return -1;
}

/**
 * The commands that match, best first. The registry's own order breaks ties, so the panel never
 * reshuffles equal matches from one keystroke to the next.
 */
export function matchCommands(
  commands: CommandInfo[],
  term: string,
  limit?: number,
): CommandInfo[] {
  const ranked = commands
    .map((command, index) => ({ command, index, rank: rankCommand(command, term) }))
    .filter((entry) => entry.rank >= 0)
    .sort((left, right) => left.rank - right.rank || left.index - right.index)
    .map((entry) => entry.command);
  return typeof limit === "number" ? ranked.slice(0, limit) : ranked;
}

/** A recent, in the shape the palette's rows use. */
export type RecentRow =
  | {
      kind: "query";
      id: string;
      label: string;
      query: string;
      resultCount: number | null;
    }
  | {
      kind: "command";
      id: string;
      label: string;
      commandId: string;
      route: string;
    };

/**
 * The stored history as rows.
 *
 * A command row needs somewhere to go: one whose route the answer did not carry (or an unknown
 * kind the API might add later) is dropped rather than rendered as a dead row.
 */
export function recentRows(recents: CommandRecent[], limit = 8): RecentRow[] {
  const rows: RecentRow[] = [];
  const seen = new Set<string>();

  for (const recent of recents) {
    if (rows.length >= limit) {
      break;
    }
    if (recent.kind === "query") {
      const query = (recent.query ?? "").trim();
      const key = `query:${query.toLowerCase()}`;
      if (!query || seen.has(key)) {
        continue;
      }
      seen.add(key);
      rows.push({
        kind: "query",
        id: key,
        label: query,
        query,
        resultCount: recent.result_count ?? null,
      });
      continue;
    }
    if (recent.kind === "command") {
      const commandId = recent.command_id ?? "";
      const route = recent.route ?? "";
      const key = `command:${commandId}`;
      if (!commandId || !route || seen.has(key)) {
        continue;
      }
      seen.add(key);
      rows.push({
        kind: "command",
        id: key,
        label: recent.title ?? commandId,
        commandId,
        route,
      });
    }
  }

  return rows;
}

/**
 * Group a suggestion list by the command's own group, preserving the order the API sent (which
 * is the registry's): the palette renders one header per group.
 */
export function groupCommands(
  commands: CommandInfo[],
): { group: string; commands: CommandInfo[] }[] {
  const groups: { group: string; commands: CommandInfo[] }[] = [];
  for (const command of commands) {
    const existing = groups.find((entry) => entry.group === command.group);
    if (existing) {
      existing.commands.push(command);
    } else {
      groups.push({ group: command.group, commands: [command] });
    }
  }
  return groups;
}

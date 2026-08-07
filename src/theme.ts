// Tiny hand-rolled ANSI helper -- deliberately not a dependency. Every
// wrapper library (chalk, picocolors, kleur...) does the same handful of
// escape codes; pulling one in just for color would be an extra supply-chain
// surface for something this small.
const wrap = (open: number | string, close: number | string) => (s: string) => `\x1b[${open}m${s}\x1b[${close}m`;

export const color = {
  bold: wrap(1, 22),
  dim: wrap(2, 22),
  cyan: wrap(36, 39),
  green: wrap(32, 39),
  yellow: wrap(33, 39),
  blue: wrap(34, 39),
  red: wrap(31, 39),
  white: wrap(97, 39),
  brightBlue: wrap(94, 39),
  // 256-color codes -- no basic ANSI slot for orange/purple/grey/dark blue.
  orange: wrap("38;5;208", 39),
  purple: wrap("38;5;135", 39),
  grey: wrap("38;5;244", 39),
  darkBlue: wrap("38;5;25", 39),
};

// One distinct color per agent so the picker reads at a glance instead of
// everyone's identical `@clack/prompts` gray-on-gray list.
const TOOL_COLORS: Record<string, (s: string) => string> = {
  claude: color.orange,
  codex: color.darkBlue,
  pi: color.yellow,
  opencode: color.grey,
  grok: color.white,
};

export function toolTag(tool: string): string {
  const c = TOOL_COLORS[tool] ?? color.white;
  return c(color.bold(`[${tool}]`));
}

export function highlightDate(dateStr: string): string {
  return color.bold(color.cyan(dateStr));
}

// What an opened tool step shows: the command it ran and its output, the file
// it read, or its arguments. Every runtime names these differently (Claude Code
// `Bash`/`Read`, OpenClaw `exec`/`read`, Hermes `terminal`/`read_file`, ACP
// `execute`/`read`), so this goes by what's in the call, not by runtime.

type Rec = Record<string, unknown>;

export function toolArgs(raw: unknown): Rec | string | null {
  if (typeof raw === "string") {
    try {
      const parsed = JSON.parse(raw);
      return parsed && typeof parsed === "object" && !Array.isArray(parsed) ? parsed : raw;
    } catch {
      return raw.trim() ? raw : null;
    }
  }
  return raw && typeof raw === "object" && !Array.isArray(raw) ? (raw as Rec) : null;
}

/** The shell command a call ran: `command` as a string, or an argv (`["bash", "-lc", script]` → script). */
export function commandOf(args: Rec | string | null): string | undefined {
  if (!args || typeof args === "string") return undefined;
  const c = args.command ?? args.cmd;
  if (typeof c === "string") return c.trim() || undefined;
  if (Array.isArray(c) && c.length && c.every((p) => typeof p === "string")) {
    const argv = c as string[];
    if (argv.length === 3 && /(^|\/)(ba|z|da)?sh$/.test(argv[0]) && /^-\w*c$/.test(argv[1])) return argv[2];
    return argv.join(" ");
  }
  return undefined;
}

/** Tool output as a person reads it: no terminal color codes, no notes meant for the model. */
export function cleanOutput(text: string): string {
  return text
    .replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, "")
    .replace(/<system-reminder>[\s\S]*?<\/system-reminder>/g, "")
    .trimEnd();
}

/**
 * A file as read tools return it, with line numbers: `12\tcode` (Claude Code),
 * `    12→code` (older Claude Code), `    12|code` (Hermes). Null when it isn't.
 */
export function numberedLines(text: string): Array<{ no?: number; text: string }> | null {
  const lines = text.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  let misses = 0;
  const out = lines.map((line) => {
    const m = /^\s*(\d+)(?:\t|→|\|)(.*)$/.exec(line);
    if (m) return { no: Number(m[1]), text: m[2] };
    misses++;
    return { text: line };
  });
  return out.length && misses <= out.length / 10 ? out : null;
}

/**
 * A long one-line command as the steps it chains, one per line, each keeping
 * its `&&` / `||` / `;` (operators inside quotes, `$( )` or `( )` don't count).
 * Scripts that already span lines, and short commands, stay as they are.
 */
export function shellLines(command: string): string[] {
  if (command.includes("\n") || command.length <= 60) return [command];
  const lines: string[] = [];
  let quote = "";
  let depth = 0;
  let start = 0;
  for (let i = 0; i < command.length; i++) {
    const c = command[i];
    if (c === "\\") {
      i++;
      continue;
    }
    if (quote) {
      if (c === quote) quote = "";
      continue;
    }
    if (c === "'" || c === '"' || c === "`") quote = c;
    else if (c === "(") depth++;
    else if (c === ")") depth = Math.max(0, depth - 1);
    else if (c === ";" && command[i + 1] === ";") i++; // `;;` ends a case branch, not a step
    else if (depth === 0) {
      const op = command.startsWith("&&", i) || command.startsWith("||", i) ? 2 : c === ";" ? 1 : 0;
      if (op) {
        lines.push(command.slice(start, i + op).trim());
        start = i + op;
        i += op - 1;
      }
    }
  }
  const rest = command.slice(start).trim();
  if (rest) lines.push(rest);
  return lines.length > 1 ? lines : [command];
}

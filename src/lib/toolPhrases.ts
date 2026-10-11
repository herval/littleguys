// Tool calls in plain words: "browsing nytimes.com" while it happens,
// "browsed nytimes.com and ran 2 commands" once it's done.
import type { ToolCallBlock } from "./transcript";

export type ToolKind =
  | "browse"
  | "fetch"
  | "websearch"
  | "search"
  | "exec"
  | "edit"
  | "read"
  | "list"
  | "memory"
  | "routine"
  | "message"
  | "ask"
  | "todo"
  | "plan"
  | "other";

function args(call: ToolCallBlock): Record<string, unknown> {
  if (typeof call.arguments === "string") {
    try {
      return JSON.parse(call.arguments) as Record<string, unknown>;
    } catch {
      return {};
    }
  }
  return (call.arguments ?? {}) as Record<string, unknown>;
}

/**
 * A tool's name as people read it: MCP tools (`mcp__<server>__<tool>`) by their
 * tool name, with the server they come from, e.g. `ab_attach` via agentbook.
 */
export function toolLabel(name: string): { name: string; via?: string } {
  const m = /^mcp__(.+?)__(.+)$/.exec(name);
  if (!m) return { name };
  // Plugin servers are `plugin_<plugin>_<server>`, often the same word twice; claude.ai connectors are `claude_ai_<App>`.
  const parts = m[1].replace(/^plugin_|^claude_ai_/, "").split("_");
  const half = parts.length / 2;
  const twice = parts.length % 2 === 0 && parts.slice(0, half).join("_") === parts.slice(half).join("_");
  return { name: m[2], via: (twice ? parts.slice(0, half) : parts).join(" ") };
}

export function toolKind(name: string): ToolKind {
  const n = name.toLowerCase();
  if (/(askuserquestion|ask_user|^clarify$|^question$)/.test(n)) return "ask";
  if (/^(todowrite|todo_write|todos?)$/.test(n)) return "todo";
  if (/(exitplanmode|^plan$|update_plan)/.test(n)) return "plan";
  if (/(^browser|navigate|open_url|playwright|chrome)/.test(n)) return "browse";
  if (/(web_search|websearch|search_web|brave|google)/.test(n)) return "websearch";
  if (/(web_fetch|fetch|http|curl)/.test(n)) return "fetch";
  if (/(search|grep|find|query|glob)/.test(n)) return "search";
  if (/(exec|bash|shell|command|terminal|process|^run)/.test(n)) return "exec";
  if (/(write|edit|patch|apply|create_file|replace|multiedit)/.test(n)) return "edit";
  if (/(^ls$|list|dir|tree)/.test(n)) return "list";
  if (/(memory|remember|recall)/.test(n)) return "memory";
  if (/(cron|schedule|automation|routine)/.test(n)) return "routine";
  if (/(message|send|post|reply|slack|telegram|whatsapp|email)/.test(n)) return "message";
  if (/(read|cat|view|file|open)/.test(n)) return "read";
  return "other";
}

function domainOf(url: unknown): string | undefined {
  if (typeof url !== "string" || !url) return undefined;
  try {
    return new URL(url.includes("://") ? url : `https://${url}`).hostname.replace(/^www\./, "");
  } catch {
    return undefined;
  }
}

function baseName(path: unknown): string | undefined {
  if (typeof path !== "string" || !path) return undefined;
  return path.replace(/\/+$/, "").split("/").pop() || path;
}

/** The thing a call acts on, short: a domain, a file name, a query. */
export function toolSubject(call: ToolCallBlock): string | undefined {
  const a = args(call);
  switch (toolKind(call.name)) {
    case "browse":
    case "fetch":
      return domainOf(a.url ?? a.targetUrl ?? a.href);
    case "websearch":
    case "search": {
      const q = [a.query, a.q, a.pattern].find((v) => typeof v === "string" && v.trim()) as string | undefined;
      return q ? (q.length > 32 ? `${q.slice(0, 30)}…` : q) : undefined;
    }
    case "edit":
    case "read":
    case "list":
      return baseName(a.path ?? a.file_path ?? a.filePath ?? a.file ?? a.dir);
    default:
      return undefined;
  }
}

const quote = (s: string) => `“${s}”`;

/** What the guy is doing right now, e.g. "browsing nytimes.com". */
export function presentTense(call: ToolCallBlock): string {
  const s = toolSubject(call);
  switch (toolKind(call.name)) {
    case "browse":
      return s ? `browsing ${s}` : "using the browser";
    case "fetch":
      return s ? `reading ${s}` : "reading a web page";
    case "websearch":
      return s ? `searching the web for ${quote(s)}` : "searching the web";
    case "search":
      return s ? `looking for ${quote(s)}` : "searching";
    case "exec":
      return "running a command";
    case "edit":
      return s ? `editing ${s}` : "editing a file";
    case "read":
      return s ? `reading ${s}` : "reading files";
    case "list":
      return s ? `looking through ${s}` : "looking through files";
    case "memory":
      return "checking their memory";
    case "routine":
      return "setting up a routine";
    case "message":
      return "sending a message";
    case "ask":
      return "waiting for your answer";
    case "todo":
      return "updating the to-do list";
    case "plan":
      return "writing up a plan";
    default:
      return `using ${toolLabel(call.name).via ?? call.name}`;
  }
}

function list(items: string[]): string {
  if (items.length <= 1) return items[0] ?? "";
  return `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`;
}

function plural(n: number, one: string, many: string): string {
  return n === 1 ? one : `${n} ${many}`;
}

/** One sentence for a run of finished steps, e.g. "Searched the web, browsed a.com and ran 2 commands". */
export function summarizeSteps(calls: ToolCallBlock[]): string {
  const groups = new Map<ToolKind, { calls: ToolCallBlock[]; subjects: string[] }>();
  for (const call of calls) {
    const kind = toolKind(call.name);
    const g = groups.get(kind) ?? { calls: [], subjects: [] };
    g.calls.push(call);
    const s = toolSubject(call);
    if (s && !g.subjects.includes(s)) g.subjects.push(s);
    groups.set(kind, g);
  }
  const parts: string[] = [];
  for (const [kind, { calls: cs, subjects }] of groups) {
    const n = cs.length;
    const named = subjects.length > 0 && subjects.length <= 2 ? list(subjects) : undefined;
    switch (kind) {
      case "browse":
        parts.push(named ? `browsed ${named}` : plural(n, "used the browser", "browser steps"));
        break;
      case "fetch":
        parts.push(named ? `read ${named}` : plural(n, "read a web page", "web pages read"));
        break;
      case "websearch":
        parts.push(n === 1 ? "searched the web" : `searched the web ${n} times`);
        break;
      case "search":
        parts.push(n === 1 ? "searched" : `ran ${n} searches`);
        break;
      case "exec":
        parts.push(n === 1 ? "ran a command" : `ran ${n} commands`);
        break;
      case "edit":
        parts.push(named ? `edited ${named}` : `edited ${n} files`);
        break;
      case "read":
        parts.push(named ? `read ${named}` : `read ${n} files`);
        break;
      case "list":
        parts.push(named ? `looked through ${named}` : "looked through some folders");
        break;
      case "memory":
        parts.push("checked their memory");
        break;
      case "routine":
        parts.push(n === 1 ? "set up a routine" : `updated ${n} routines`);
        break;
      case "message":
        parts.push(n === 1 ? "sent a message" : `sent ${n} messages`);
        break;
      case "ask":
        parts.push(n === 1 ? "asked you a question" : `asked you ${n} times`);
        break;
      case "todo":
        parts.push("updated the to-do list");
        break;
      case "plan":
        parts.push("proposed a plan");
        break;
      default: {
        // MCP tools go by their server: "used agentbook", not each of its tools.
        const names = [...new Set(cs.map((c) => toolLabel(c.name).via ?? c.name))];
        parts.push(`used ${list(names.slice(0, 3))}${names.length > 3 ? ` and ${names.length - 3} more` : ""}`);
      }
    }
  }
  const sentence = list(parts);
  return sentence.charAt(0).toUpperCase() + sentence.slice(1);
}

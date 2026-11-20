// What littleguys does on top of `agents.create` to give a new guy sensible
// defaults: a filled-in identity, an optional personality, and its own
// managed browser profile. All of it uses OpenClaw's own files and config.
import { agents, config } from "./gateway";
import { readEditableFile, writeEditedFile } from "./fileEdit";

const PERSONALITY_HEADING = "## Personality (from littleguys)";
const BROWSER_HEADING = "## Browser (from littleguys)";

export function identityMarkdown(name: string, emoji: string | null, vibe: string | null): string {
  return [
    "# IDENTITY.md - Who Am I?",
    "",
    `- **Name:** ${name}`,
    ...(emoji ? [`- **Emoji:** ${emoji}`] : []),
    ...(vibe ? [`- **Vibe:** ${vibe}`] : []),
    "",
    "_Set up in littleguys. Edit freely — this is yours._",
    "",
  ].join("\n");
}

/** Replace (or append) a littleguys-managed `## …` section in a markdown file. */
export function upsertSection(doc: string, heading: string, body: string | null): string {
  const lines = doc.split("\n");
  const start = lines.findIndex((l) => l.trim() === heading);
  let end = lines.length;
  if (start !== -1) {
    for (let i = start + 1; i < lines.length; i++) {
      if (/^#{1,2} /.test(lines[i])) {
        end = i;
        break;
      }
    }
  }
  const section = body && body.trim() ? [heading, "", body.trim(), ""] : [];
  if (start === -1) {
    if (!section.length) return doc;
    return `${doc.replace(/\s*$/, "")}\n\n${section.join("\n")}`;
  }
  return [...lines.slice(0, start), ...section, ...lines.slice(end)].join("\n").replace(/\n{3,}/g, "\n\n");
}

export function readSection(doc: string, heading: string): string {
  const lines = doc.split("\n");
  const start = lines.findIndex((l) => l.trim() === heading);
  if (start === -1) return "";
  const out: string[] = [];
  for (let i = start + 1; i < lines.length; i++) {
    if (/^#{1,2} /.test(lines[i])) break;
    out.push(lines[i]);
  }
  return out.join("\n").trim();
}

export async function readPersonality(gw: string, agentId: string): Promise<string> {
  const res = await agents.getFile(gw, agentId, "SOUL.md").catch(() => null);
  return res?.file?.content ? readSection(res.file.content, PERSONALITY_HEADING) : "";
}

export async function writePersonality(gw: string, agentId: string, personality: string): Promise<void> {
  const file = await readEditableFile(gw, agentId, "SOUL.md", "# SOUL.md - Who You Are\n");
  const current = file.content;
  const next = upsertSection(current, PERSONALITY_HEADING, personality);
  await writeEditedFile(gw, agentId, "SOUL.md", file, next);
}

export function browserProfileName(agentId: string): string {
  return agentId.toLowerCase().replace(/[^a-z0-9-]/g, "-").replace(/^-+|-+$/g, "") || "guy";
}

/**
 * Give the guy its own managed Chrome profile (separate cookies/logins), and
 * tell it to use it. OpenClaw has no per-agent browser binding, so this is a
 * named profile + an AGENTS.md instruction.
 */
export async function giveOwnBrowser(gw: string, agentId: string, displayName: string): Promise<string> {
  const file = await readEditableFile(gw, agentId, "AGENTS.md");
  const snapshot = await config.get(gw);
  const parsed = (snapshot.parsed ?? snapshot.config ?? {}) as {
    gateway?: { port?: number };
    browser?: { profiles?: Record<string, { cdpPort?: number }> };
  };
  const profile = browserProfileName(agentId);
  const existing = parsed.browser?.profiles ?? {};
  if (!existing[profile]) {
    if (!snapshot.hash) throw new Error("Couldn't read browser configuration safely. Refresh and try again.");
    const base = parsed.gateway?.port ?? 18789;
    const used = new Set(Object.values(existing).map((p) => p.cdpPort).filter(Boolean));
    // Managed CDP ports live at gateway+11 … gateway+110; +11 is the default profile.
    let port = base + 12;
    while (used.has(port) && port < base + 110) port++;
    await config.patch(
      gw,
      { browser: { profiles: { [profile]: { cdpPort: port, driver: "openclaw" } } } },
      snapshot.hash,
      `littleguys: browser profile for ${displayName}`,
    );
  }
  const current = file.content;
  const next = upsertSection(
    current,
    BROWSER_HEADING,
    `You have your own browser. Whenever you use the browser tool, always pass \`profile="${profile}"\` so you use your own window, cookies and logins — never another guy's.`,
  );
  await writeEditedFile(gw, agentId, "AGENTS.md", file, next);
  return profile;
}

export async function hasOwnBrowser(gw: string, agentId: string): Promise<boolean> {
  const file = await agents.getFile(gw, agentId, "AGENTS.md").catch(() => null);
  return Boolean(file?.file?.content && readSection(file.file.content, BROWSER_HEADING));
}

export const PERSONALITIES: Array<{ id: string; label: string; emoji: string; text: string }> = [
  {
    id: "assistant",
    label: "Helpful assistant",
    emoji: "🙂",
    text: "Warm, quick and practical. Gets things done without fuss and keeps replies short unless asked for detail.",
  },
  {
    id: "researcher",
    label: "Researcher",
    emoji: "🔎",
    text: "Curious and thorough. Digs through sources, cross-checks facts, cites where things came from, and flags uncertainty plainly.",
  },
  {
    id: "builder",
    label: "Builder",
    emoji: "🛠️",
    text: "A hands-on engineer. Prefers working code over talk, explains trade-offs briefly, and tests before declaring something done.",
  },
  {
    id: "butler",
    label: "Butler",
    emoji: "🎩",
    text: "Discreet and impeccably organized. Anticipates needs, keeps track of errands and reminders, and reports back crisply.",
  },
  {
    id: "buddy",
    label: "Buddy",
    emoji: "🐣",
    text: "Playful and upbeat, with a dry sense of humor. Still competent — the jokes never get in the way of the work.",
  },
];

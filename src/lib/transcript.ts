// Turning gateway transcripts into render items. Transcripts mix OpenClaw's
// own block shapes with ones imported from CLI runtimes (e.g. Claude CLI
// history: `toolcall`, `tool_result`); everything is normalized here.
import { cleanPreview } from "./guys";
import type { ChatMessage, ContentBlock } from "./types";

export interface ToolCallBlock {
  id: string;
  name: string;
  arguments?: unknown;
}

export interface ToolResult {
  text: string;
  isError: boolean;
}

export interface MediaRef {
  kind: "image" | "video" | "audio" | "file";
  artifactId?: string;
  url?: string;
  data?: string;
  mimeType?: string;
  fileName?: string;
  sizeBytes?: number;
  width?: number;
  height?: number;
  /** A file the bot referenced by path on its own machine (`MEDIA:/…`). */
  path?: string;
}

export type Item =
  | { kind: "user"; id: string; text: string; media: MediaRef[]; sender?: string; ts?: number; pending?: boolean }
  | { kind: "text"; id: string; text: string; ts?: number }
  | { kind: "thinking"; id: string; text: string; ts?: number }
  | { kind: "tool"; id: string; call: ToolCallBlock; result?: ToolResult; ts?: number }
  | { kind: "media"; id: string; media: MediaRef; ts?: number }
  | { kind: "note"; id: string; text: string; tone: "muted" | "error"; ts?: number };

const HIDDEN_REPLIES = new Set(["NO_REPLY", "HEARTBEAT_OK"]);
const OVERSIZED = /^\[[^\]]*history record omitted from context because it exceeded [^\]]*\]$/i;
const MEDIA_LINE = /^[ \t]*MEDIA:[ \t]*(\S[^\n]*?)[ \t]*$/gim;

function rawBlocks(m: ChatMessage): ContentBlock[] {
  if (typeof m.content === "string") return m.content ? [{ type: "text", text: m.content }] : [];
  return Array.isArray(m.content) ? m.content.filter((b): b is ContentBlock => Boolean(b) && typeof b === "object") : [];
}

type Rec = Record<string, unknown>;
const str = (v: unknown): string | undefined => (typeof v === "string" && v ? v : undefined);
const num = (v: unknown): number | undefined => (typeof v === "number" && Number.isFinite(v) ? v : undefined);

function resultText(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content
      .map((c) => {
        if (typeof c === "string") return c;
        const r = c as Rec;
        if (r?.type === "text" && typeof r.text === "string") return r.text;
        if (r?.type === "image") return "[image]";
        return "";
      })
      .filter(Boolean)
      .join("\n");
  }
  return "";
}

function mediaFrom(b: Rec): MediaRef | null {
  const type = str(b.type);
  const att = (b.attachment as Rec | undefined) ?? b;
  const mime = str(att.mimeType) ?? str((b.source as Rec | undefined)?.media_type);
  const kindFromMime = (m?: string): MediaRef["kind"] =>
    m?.startsWith("image/") ? "image" : m?.startsWith("video/") ? "video" : m?.startsWith("audio/") ? "audio" : "file";
  let kind: MediaRef["kind"];
  if (type === "image") kind = "image";
  else if (type === "video") kind = "video";
  else if (type === "audio") kind = "audio";
  else if (type === "attachment" || type === "file" || type === "document") kind = kindFromMime(mime);
  else return null;
  return {
    kind,
    artifactId: str(att.artifactId) ?? str(att.id),
    url: str(att.url) ?? str(att.openUrl),
    data: str(b.data) ?? str((b.source as Rec | undefined)?.data),
    mimeType: mime,
    fileName: str(att.fileName) ?? str(att.label) ?? str(att.title) ?? str(b.alt),
    sizeBytes: num(att.sizeBytes),
    width: num(b.width),
    height: num(b.height),
  };
}

/** Split `MEDIA:/path` directive lines out of message text. */
export function extractMediaLines(text: string): { text: string; files: MediaRef[] } {
  const files: MediaRef[] = [];
  const rest = text.replace(MEDIA_LINE, (_, path: string) => {
    const clean = path.replace(/^["'<]|["'>]$/g, "");
    files.push({ kind: kindFromName(clean), path: clean, fileName: clean.split(/[\\/]/).pop() || clean });
    return "";
  });
  return { text: rest.replace(/\n{3,}/g, "\n\n").trim(), files };
}

function kindFromName(name: string): MediaRef["kind"] {
  const ext = name.toLowerCase().split(".").pop() ?? "";
  if (["png", "jpg", "jpeg", "gif", "webp", "heic", "svg"].includes(ext)) return "image";
  if (["mp4", "mov", "webm", "m4v"].includes(ext)) return "video";
  if (["mp3", "m4a", "wav", "ogg", "opus", "aac", "flac"].includes(ext)) return "audio";
  return "file";
}

function isInternal(m: ChatMessage): boolean {
  const provenance = (m as { provenance?: { kind?: string } }).provenance;
  if (provenance?.kind === "internal_system") return true;
  const text = rawBlocks(m)
    .filter((b) => b.type === "text")
    .map((b) => (b as { text: string }).text)
    .join("")
    .trim();
  return m.role === "user" && /^\[OpenClaw [^\]]*poll\]$/.test(text);
}

export function cleanDirectives(text: string): string {
  return text.replace(/^\s*(\[\[[a-z_:\- ]+\]\]\s*)+/i, "");
}

export function toItems(messages: ChatMessage[], pending: ChatMessage[]): Item[] {
  // Tool results arrive as `toolResult` messages (OpenClaw) or `tool_result`
  // blocks inside user/assistant messages (Claude CLI history).
  const results = new Map<string, ToolResult>();
  for (const m of messages) {
    if (m.role === "toolResult" && m.toolCallId) {
      results.set(m.toolCallId, { text: resultText(m.content), isError: Boolean(m.isError) });
    }
    for (const b of rawBlocks(m)) {
      const r = b as Rec;
      if (r.type === "tool_result" && typeof r.tool_use_id === "string") {
        results.set(r.tool_use_id, { text: resultText(r.content), isError: Boolean(r.is_error) });
      }
    }
  }

  const items: Item[] = [];
  messages.forEach((m, i) => {
    const id = m.__openclaw?.id ?? `m${i}`;
    if (isInternal(m) || m.role === "toolResult") return;
    const bs = rawBlocks(m);

    // Oversized records the gateway replaced with a placeholder.
    const onlyText = bs.every((b) => b.type === "text");
    const joined = bs.map((b) => (b.type === "text" ? (b as { text: string }).text : "")).join("").trim();
    if (onlyText && OVERSIZED.test(joined)) {
      items.push({ kind: "note", id, text: "Something large here (a file or a long output) was too big to load.", tone: "muted", ts: m.timestamp });
      return;
    }

    if (m.role === "user") {
      // CLI runtimes record tool output as user messages — those are paired above.
      if (bs.length > 0 && bs.every((b) => b.type === "tool_result")) return;
      const texts: string[] = [];
      const media: MediaRef[] = [];
      for (const b of bs) {
        if (b.type === "text") {
          const { text, files } = extractMediaLines((b as { text: string }).text);
          if (text) texts.push(text);
          media.push(...files);
        } else {
          const ref = mediaFrom(b as Rec);
          if (ref) media.push(ref);
        }
      }
      const sender = str((m as unknown as Rec).senderLabel);
      if (!texts.length && !media.length) return;
      items.push({ kind: "user", id, text: texts.join("\n"), media, sender, ts: m.timestamp });
      return;
    }

    if (m.role === "assistant") {
      const ts = m.timestamp;
      bs.forEach((b, j) => {
        const bid = `${id}:${j}`;
        const r = b as Rec;
        const type = str(r.type);
        if (type === "text") {
          const { text, files } = extractMediaLines(cleanDirectives((b as { text: string }).text));
          if (text && !HIDDEN_REPLIES.has(text.trim())) items.push({ kind: "text", id: bid, text, ts });
          files.forEach((f, k) => items.push({ kind: "media", id: `${bid}:f${k}`, media: f, ts }));
        } else if (type === "thinking" || type === "reasoning") {
          const text = String(r.thinking ?? r.text ?? "").trim();
          if (text) items.push({ kind: "thinking", id: bid, text, ts });
        } else if (type === "toolCall" || type === "toolcall" || type === "tool_use") {
          const call: ToolCallBlock = { id: String(r.id ?? bid), name: String(r.name ?? "tool"), arguments: r.arguments ?? r.input };
          items.push({ kind: "tool", id: bid, call, result: results.get(call.id), ts });
        } else if (type !== "tool_result") {
          const ref = mediaFrom(r);
          if (ref) items.push({ kind: "media", id: bid, media: ref, ts });
        }
      });
      if (m.stopReason === "error") {
        items.push({ kind: "note", id: `${id}:err`, text: "This reply hit an error.", tone: "error", ts });
      }
      return;
    }

    // system / custom notices
    const display = str((m as unknown as Rec).display);
    const text = display ?? bs.filter((b) => b.type === "text").map((b) => (b as { text: string }).text).join(" ");
    if (text) items.push({ kind: "note", id, text: cleanPreview(text), tone: "muted", ts: m.timestamp });
  });

  pending.forEach((m, i) => {
    const bs = rawBlocks(m);
    items.push({
      kind: "user",
      id: `pending-${i}`,
      text: bs.filter((b) => b.type === "text").map((b) => (b as { text: string }).text).join("\n"),
      media: bs.map((b) => mediaFrom(b as Rec)).filter((x): x is MediaRef => Boolean(x)),
      ts: m.timestamp,
      pending: true,
    });
  });
  return items;
}

/**
 * Where you left off in a thread that came back unread: the first item newer
 * than `lastReadAt`, other than your own messages. Without a read time (or with
 * nothing newer, as when you marked it unread yourself), the guy's latest reply.
 * -1 when there's nothing before it to set apart.
 */
export function firstUnread(items: Item[], lastReadAt?: number): number {
  const mine = (it: Item) => it.kind === "user" && !it.sender;
  let at = lastReadAt ? items.findIndex((it) => !mine(it) && (it.ts ?? 0) > lastReadAt) : -1;
  if (at < 0) {
    const lastMine = items.findLastIndex(mine);
    at = lastMine + 1 < items.length ? lastMine + 1 : -1;
  }
  return at > 0 ? at : -1;
}

// Turning gateway transcripts into render items. Transcripts mix OpenClaw's
// own block shapes with ones imported from CLI runtimes (e.g. Claude CLI
// history: `toolcall`, `tool_result`); everything is normalized here.
import { cleanPreview } from "./guys";
import { parseInbound } from "./inbound";
import type { ChatMessage, ContentBlock, ToolDiff } from "./types";

export interface ToolCallBlock {
  id: string;
  name: string;
  arguments?: unknown;
}

export interface ToolResult {
  text: string;
  isError: boolean;
  /** Files the tool changed, as the runtime recorded them. */
  diffs?: ToolDiff[];
  /** Images (and other media) the tool returned, e.g. a screenshot or a file it read. */
  media?: MediaRef[];
  /** A question tool's answers: question → answer. */
  answers?: Record<string, string>;
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
  /** Only mentioned in the text: shown if it can be had, else left out. */
  mentioned?: boolean;
}

export type Item =
  | { kind: "user"; id: string; text: string; media: MediaRef[]; sender?: string; via?: string; ts?: number; pending?: boolean }
  /** `media`: images, videos and audio the text mentions by path or link. */
  | { kind: "text"; id: string; text: string; media?: MediaRef[]; ts?: number }
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
        return ""; // images: see resultMedia
      })
      .filter(Boolean)
      .join("\n");
  }
  return "";
}

function resultMedia(content: unknown): MediaRef[] | undefined {
  if (!Array.isArray(content)) return undefined;
  const media = content.map((c) => (c && typeof c === "object" ? mediaFrom(c as Rec) : null)).filter((m): m is MediaRef => Boolean(m));
  return media.length ? media : undefined;
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

// What media.rs will serve from a local guy's disk.
const IMAGE_EXT = ["png", "jpg", "jpeg", "gif", "webp", "heic", "heif", "avif", "bmp", "svg"];
const VIDEO_EXT = ["mp4", "m4v", "mov", "webm"];
const AUDIO_EXT = ["mp3", "m4a", "aac", "wav", "ogg", "opus", "flac"];
const MEDIA_EXT = [...IMAGE_EXT, ...VIDEO_EXT, ...AUDIO_EXT].join("|");

function kindFromName(name: string): MediaRef["kind"] {
  const ext = name.toLowerCase().split(/[?#]/)[0].split(".").pop() ?? "";
  if (IMAGE_EXT.includes(ext)) return "image";
  if (VIDEO_EXT.includes(ext)) return "video";
  if (AUDIO_EXT.includes(ext)) return "audio";
  return "file";
}

const MEDIA_NAME = new RegExp(`\\.(?:${MEDIA_EXT})$`, "i");
const MEDIA_URL = new RegExp(`^https?://[^\\s?#]+\\.(?:${MEDIA_EXT})(?:[?#]\\S*)?$`, "i");
const FENCE = /^ {0,3}(`{3,}|~{3,})[^\n]*\n[\s\S]*?(?:^ {0,3}\1[`~]*[ \t]*$|(?![\s\S]))/gm;
const CODE_SPAN = /(`+)([^`\n]+?)\1/g;
const LINK = /!?\[[^\]\n]*\]\(\s*(?:<([^>\n]+)>|([^)\s]+))(?:\s+"[^"]*")?\s*\)/g;
// A path or URL in prose: starts after a space or an opening mark, ends before closing punctuation.
const BARE = /(^|[\s(\[<"'*_])((?:https?:\/\/|file:\/\/|~\/|\.{1,2}\/|\/)[^\s`"'<>()[\]]+)/gm;

/**
 * An image, video or audio file `target` points at: a path on the guy's
 * machine (absolute, `~/…`, `file://…` or relative to its folder) or an
 * http(s) URL. Null for anything else.
 */
export function mediaTarget(target: string): MediaRef | null {
  const t = target.trim();
  if (!t || t.length > 1024 || /[\n`]/.test(t)) return null;
  const name = decode(t.split(/[?#]/)[0].split(/[\\/]/).pop() || t);
  if (/^https?:/i.test(t)) return MEDIA_URL.test(t) ? { kind: kindFromName(t), url: t, fileName: name, mentioned: true } : null;
  if (/^[a-z][a-z\d+.-]*:/i.test(t) && !/^file:/i.test(t)) return null; // mailto:, data:, …
  if (!MEDIA_NAME.test(t)) return null;
  return { kind: kindFromName(t), path: t, fileName: name, mentioned: true };
}

const NOT_A_PATH = /[(){}<>|;&$*?="'\\]/;
const ROOTED = /^(?:~|\.{1,2})?\//;

/**
 * The file or folder `target` names, if it looks like a path: absolute,
 * `~/…`, `file://…`, or relative to the guy's folder (`src/main.rs`,
 * `README.md`), without a `:12` or `#L12` line suffix. Null for URLs,
 * commands, code and plain words; whether it's really there is for the
 * caller to find out.
 */
export function pathTarget(target: string, { linked = false } = {}): string | null {
  let t = target.trim();
  if (/^file:\/\//i.test(t)) t = decode(t.replace(/^file:\/\/(?:localhost)?/i, ""));
  if (!t || t.length > 1024 || /[\n`]/.test(t) || NOT_A_PATH.test(t)) return null;
  t = t.replace(/#L\d+(?:-L?\d+)?$/, "").replace(/:\d+(?::\d+)?$/, "");
  if (/^[a-z][a-z\d+.-]*:/i.test(t) || t.startsWith("//")) return null; // a URL
  if (!linked && /\s/.test(t) && !ROOTED.test(t)) return null; // `open out/clip.mp4` is a command
  if (ROOTED.test(t) || t.includes("/")) return /[^/~.]/.test(t) ? t : null;
  // A bare name: a file with an extension or a dotfile, but not a version number.
  return /^[\w@+-][\w@.+-]*\.[A-Za-z]\w{0,9}$|^\.\w[\w.-]*$/.test(t) ? t : null;
}

/** The path a Markdown link points at (its target is a URL, so the path is percent-encoded). */
export function linkedPath(href: string): string | null {
  return /^https?:/i.test(href) ? null : pathTarget(decode(href), { linked: true });
}

/** Paths in prose (no Markdown) that start at a root (`/`, `~/`, `./`): where each starts, as written. */
export function barePaths(text: string): { at: number; target: string }[] {
  const found: { at: number; target: string }[] = [];
  for (const m of text.matchAll(BARE)) {
    const target = m[2].replace(/[.,;:!?*_]+$/, "");
    if (!/^https?:/i.test(target) && pathTarget(target) === target) found.push({ at: (m.index ?? 0) + m[1].length, target });
  }
  return found;
}

/** The media a Markdown link or image points at; its target is a URL, so a path in it is percent-encoded. */
function linkedMedia(href: string): MediaRef | null {
  return mediaTarget(/^https?:/i.test(href) ? href : decode(href));
}

function decode(s: string): string {
  try {
    return decodeURIComponent(s);
  } catch {
    return s;
  }
}

/** Paths and URLs to media in prose (no Markdown): where each starts, as written, and what it points at. */
function bareMedia(text: string): { at: number; target: string; ref: MediaRef }[] {
  const found: { at: number; target: string; ref: MediaRef }[] = [];
  for (const m of text.matchAll(BARE)) {
    const target = m[2].replace(/[.,;:!?*_]+$/, "");
    const ref = mediaTarget(target);
    if (ref) found.push({ at: (m.index ?? 0) + m[1].length, target, ref });
  }
  return found;
}

/**
 * The media a reply mentions, in order: in code spans (`~/out/clip.mp4`),
 * as link or image targets, or as bare paths and URLs. Code blocks are
 * skipped (command output isn't a mention).
 */
export function mentionedMedia(text: string, limit = 4): MediaRef[] {
  const blank = (m: string) => " ".repeat(m.length);
  const found: { at: number; ref: MediaRef }[] = [];
  const add = (ref: MediaRef | null, at: number) => ref && found.push({ at, ref });
  let rest = text.replace(FENCE, blank);
  rest = rest.replace(CODE_SPAN, (m, _ticks: string, code: string, at: number) => {
    // `open out/clip.mp4` is a command; `~/Movies/My clip.mov` is a path.
    if (!/\s/.test(code.trim()) || /^\s*(?:~|\.{0,2})\/|^\s*file:/i.test(code)) add(mediaTarget(code), at);
    return blank(m);
  });
  rest = rest.replace(LINK, (m, bracketed: string | undefined, plain: string | undefined, at: number) => {
    add(linkedMedia(bracketed ?? plain ?? ""), at);
    return blank(m);
  });
  for (const b of bareMedia(rest)) add(b.ref, b.at);

  const seen = new Set<string>();
  return found
    .sort((a, b) => a.at - b.at)
    .map((f) => f.ref)
    .filter((ref) => {
      const key = ref.url ?? ref.path ?? "";
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    })
    .slice(0, limit);
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
      results.set(m.toolCallId, { text: resultText(m.content), isError: Boolean(m.isError), diffs: m.diffs, media: resultMedia(m.content), answers: m.answers });
    }
    for (const b of rawBlocks(m)) {
      const r = b as Rec;
      if (r.type === "tool_result" && typeof r.tool_use_id === "string") {
        results.set(r.tool_use_id, { text: resultText(r.content), isError: Boolean(r.is_error), media: resultMedia(r.content) });
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
      const events: string[] = [];
      let sender = str(m.senderLabel);
      let via: string | undefined;
      for (const b of bs) {
        if (b.type === "text") {
          // Channel messages carry the events OpenClaw queued for the guy;
          // those show as notes, and the one announcing this message as its sender.
          const inbound = parseInbound((b as { text: string }).text);
          sender ??= inbound.sender;
          via ??= inbound.via;
          events.push(...inbound.events.filter((e) => !events.includes(e)));
          const { text, files } = extractMediaLines(inbound.body);
          if (text) texts.push(text);
          media.push(...files);
        } else {
          const ref = mediaFrom(b as Rec);
          if (ref) media.push(ref);
        }
      }
      events.forEach((text, k) => items.push({ kind: "note", id: `${id}:e${k}`, text, tone: "muted", ts: m.timestamp }));
      if (!texts.length && !media.length) return;
      items.push({ kind: "user", id, text: texts.join("\n"), media, sender, via, ts: m.timestamp });
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
          if (text && !HIDDEN_REPLIES.has(text.trim())) {
            const attached = new Set(files.map((f) => f.path));
            const media = mentionedMedia(text).filter((m) => !m.path || !attached.has(m.path));
            items.push({ kind: "text", id: bid, text, media: media.length ? media : undefined, ts });
          }
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

// Cmd+K: find a guy, a thread or a message on any gateway, as you type.
// Guys and threads match right away, from the roster and the archived threads
// already loaded; messages come from `sessions.search`: OpenClaw's own index, and
// littleguys' index of Claude Code, Codex and ACP transcripts (search.rs).
import { sessions as sessionsApi } from "./gateway";
import { capsOf, channelName, cleanPreview, gatewayOf, guyName, hasMainChat, isMultiGateway, sessionsFor, threadKind, threadTitle } from "./guys";
import type { RpcError } from "./ipc";
import type { Guy, MessageHit, PastSession, Roster, RuntimeKind, SessionRow } from "./types";

/** What shows before anything's typed: the latest threads anywhere. */
const RECENT = 8;
const MAX_GUYS = 4;
const MAX_THREADS = 30;
const MAX_MESSAGES = 12;
/** The most threads one `sessions.search` looks through (OpenClaw). */
const MAX_SEARCH_KEYS = 200;
/** Runtimes littleguys indexes itself: every thread is searched, listed or not. */
const INDEXED: ReadonlySet<RuntimeKind> = new Set(["claudecode", "codex", "acp"]);

/** Lowercase and without accents, so "sao" finds "São". */
export function fold(text: string): string {
  return text.normalize("NFD").replace(/\p{M}/gu, "").toLowerCase();
}

export function queryWords(query: string): string[] {
  return fold(query).split(/\s+/).filter(Boolean);
}

export interface GuyResult {
  guy: Guy;
  score: number;
}

export interface ThreadResult {
  guy: Guy;
  row: SessionRow;
  title: string;
  /** The last message, cleaned up. */
  preview: string;
  /** The folder it works in (Claude Code, Codex, ACP): its last part. */
  folder: string | null;
  archived: boolean;
  /** The guy's main chat, which is just "the guy". */
  main: boolean;
  score: number;
}

export interface MessageResult {
  guy: Guy;
  key: string;
  title: string;
  archived: boolean;
  hit: MessageHit;
  /** A session that isn't a thread in the list: `sessions.open` makes it one. */
  past?: Pick<PastSession, "sessionId" | "cwd" | "title" | "updatedAt">;
}

type Thread = Omit<ThreadResult, "score">;

/** Every guy's threads (not their helpers), then the archived ones that are loaded. */
function listThreads(roster: Roster, guys: Guy[], archivedByGuy: Record<string, SessionRow[] | undefined>): Thread[] {
  const out: Thread[] = [];
  for (const guy of guys) {
    const withMain = hasMainChat(roster, guy.gatewayId);
    const live = sessionsFor(roster, guy).filter((r) => !r.archived);
    const seen = new Set(live.map((r) => r.key));
    const rows = [
      ...live.map((row) => ({ row, archived: false })),
      ...(archivedByGuy[guy.uid] ?? []).filter((r) => !seen.has(r.key)).map((row) => ({ row, archived: true })),
    ];
    for (const { row, archived } of rows) {
      const kind = threadKind(row.key);
      if (kind === "helper") continue;
      // An ACP guy's draft isn't a thread until its first message.
      if (kind === "main" && !withMain && !row.updatedAt) continue;
      out.push({
        guy,
        row,
        archived,
        title: threadTitle(row, withMain),
        preview: cleanPreview(row.lastMessagePreview),
        folder: folderName(row.workspaceDir),
        main: kind === "main" && withMain,
      });
    }
  }
  return out;
}

function folderName(dir: string | null | undefined): string | null {
  return dir?.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || null;
}

const newestFirst = (a: Thread, b: Thread) => (b.row.updatedAt ?? 0) - (a.row.updatedAt ?? 0);

/** How well `word` sits in `field`: at the start of a word beats inside one. */
function wordScore(field: string, word: string, weight: number): number {
  let at = field.indexOf(word);
  if (at < 0) return 0;
  for (; at >= 0; at = field.indexOf(word, at + 1)) {
    if (at === 0 || !/[\p{L}\p{N}]/u.test(field[at - 1])) return weight;
  }
  return weight / 2;
}

/** Every word has to be in some field, and counts where it fits best. 0: no match. */
function score(fields: Array<[string, number]>, words: string[]): number {
  let total = 0;
  for (const word of words) {
    let best = 0;
    for (const [field, weight] of fields) best = Math.max(best, wordScore(field, word, weight));
    if (!best) return 0;
    total += best;
  }
  return total;
}

/** Guys and threads matching `query`, best first; with nothing typed, the latest threads. */
export function findInRoster(
  roster: Roster,
  guys: Guy[],
  archivedByGuy: Record<string, SessionRow[] | undefined>,
  query: string,
): { guys: GuyResult[]; threads: ThreadResult[] } {
  const threads = listThreads(roster, guys, archivedByGuy);
  const words = queryWords(query);
  if (!words.length) {
    const recent = threads.filter((t) => !t.archived && t.row.updatedAt && threadKind(t.row.key) !== "routine");
    return { guys: [], threads: recent.sort(newestFirst).slice(0, RECENT).map((t) => ({ ...t, score: 0 })) };
  }
  const whole = fold(query.trim());
  const multi = isMultiGateway(roster);
  const found = guys
    .map((guy) => {
      const name = fold(guyName(guy));
      const fields: Array<[string, number]> = [[name, 100]];
      if (multi) fields.push([fold(guy.gatewayName), 40]);
      const s = score(fields, words);
      return { guy, score: s && (name.startsWith(whole) ? s + 50 : s) };
    })
    .filter((r) => r.score > 0)
    .sort((a, b) => b.score - a.score)
    .slice(0, MAX_GUYS);
  const shown = new Set(found.map((r) => r.guy.uid));
  const matched = threads
    // A guy who's found already stands for their main chat.
    .filter((t) => !(t.main && shown.has(t.guy.uid)))
    .map((t) => {
      const title = fold(t.title);
      const channel = threadKind(t.row.key) === "channel" ? channelName(t.row.key) : null;
      let s = score(
        [
          [title, 100],
          [fold(guyName(t.guy)), 45],
          [fold(t.folder ?? ""), 40],
          [fold(channel ?? ""), 35],
          [fold(t.preview), 25],
        ],
        words,
      );
      if (s && title.startsWith(whole)) s += 50;
      // Archived threads still turn up, below the ones in use.
      return { ...t, score: t.archived ? s * 0.85 : s };
    })
    .filter((t) => t.score > 0)
    .sort((a, b) => b.score - a.score || newestFirst(a, b))
    .slice(0, MAX_THREADS);
  return { guys: found, threads: matched };
}

/** Gateways that said they can't search messages; they aren't asked again. */
const cantSearch = new Set<string>();

function isUnsupported(e: unknown): boolean {
  const err = (e ?? {}) as Partial<RpcError>;
  return err.code === "UNSUPPORTED" || /unknown method|not supported|isn't supported/i.test(err.message ?? "");
}

/**
 * Messages matching `query`, from every guy whose gateway can search them: the
 * best hit per thread, best first. `indexing`: some messages aren't searchable yet.
 */
export async function findMessages(
  roster: Roster,
  guys: Guy[],
  archivedByGuy: Record<string, SessionRow[] | undefined>,
  query: string,
): Promise<{ results: MessageResult[]; indexing: boolean }> {
  const q = query.trim();
  const threads = listThreads(roster, guys, archivedByGuy);
  let indexing = false;
  const perGuy = await Promise.all(
    guys
      .filter((guy) => !cantSearch.has(guy.gatewayId))
      .map(async (guy) => {
        const mine = threads.filter((t) => t.guy.uid === guy.uid).sort(newestFirst);
        const indexed = INDEXED.has(gatewayOf(roster, guy.gatewayId)?.status.kind ?? "openclaw");
        if (!mine.length && !indexed) return [];
        const keys = indexed ? [] : mine.slice(0, MAX_SEARCH_KEYS).map((t) => t.row.key);
        const canOpen = capsOf(roster, guy.gatewayId).pastSessions;
        let res: Awaited<ReturnType<typeof sessionsApi.search>>;
        try {
          res = await sessionsApi.search(guy.gatewayId, guy.id, keys, q);
        } catch (e) {
          if (isUnsupported(e)) cantSearch.add(guy.gatewayId);
          return [];
        }
        indexing ||= Boolean(res.indexing);
        const withMain = hasMainChat(roster, guy.gatewayId);
        return (res.results ?? [])
          .filter((hit) => threadKind(hit.sessionKey) !== "helper")
          .map((hit): MessageResult => {
            const t = mine.find((t) => t.row.key === hit.sessionKey);
            const title = t?.title ?? hit.title ?? threadTitle({ key: hit.sessionKey, updatedAt: hit.timestamp }, withMain);
            const updatedAt = hit.updatedAt ?? hit.timestamp;
            return {
              guy,
              key: hit.sessionKey,
              title,
              archived: t?.archived ?? false,
              hit,
              past: !t && canOpen ? { sessionId: hit.sessionId, cwd: hit.workspaceDir ?? "", title, updatedAt } : undefined,
            };
          });
      }),
  );
  const seen = new Set<string>();
  const results = perGuy
    .flat()
    .sort((a, b) => b.hit.score - a.hit.score || b.hit.timestamp - a.hit.timestamp)
    .filter((r) => {
      const id = `${r.guy.uid}|${r.key}`;
      if (seen.has(id)) return false;
      seen.add(id);
      return true;
    })
    .slice(0, MAX_MESSAGES);
  return { results, indexing };
}

/** A snippet of a message without its markdown: no fences, backticks or emphasis marks. */
export function plainSnippet(text: string): string {
  return text
    // An attachment (OpenClaw's MEDIA: line) is just its file name, as in the chat.
    .replace(/\bMEDIA:(\S+)/g, (_, path: string) => path.split(/[\\/]/).pop() ?? path)
    .replace(/```[\w-]*/g, " ")
    .replace(/`+/g, "")
    // Emphasis marks only at the edges of words, so snake_case and __init__ stay.
    .replace(/(^|[\s(])(\*\*|~~|\*|_)(?=[^\s_*])/g, "$1")
    .replace(/(?<=[^\s_*])(\*\*|~~|\*|_)(?=$|[\s).,;:!?])/g, "")
    .replace(/^#+\s+|(\s)#+\s+/g, "$1")
    .replace(/\s+/g, " ")
    .trim();
}

/** `text` cut into the parts that match a query word and the parts between. */
export function highlightParts(raw: string, words: string[]): Array<{ text: string; match: boolean }> {
  if (!words.length || !raw) return [{ text: raw, match: false }];
  const text = raw.normalize("NFC");
  // Fold each character on its own, keeping where it came from.
  let folded = "";
  const from: number[] = [];
  for (let i = 0; i < text.length; ) {
    const ch = String.fromCodePoint(text.codePointAt(i)!);
    const f = fold(ch);
    for (let k = 0; k < f.length; k++) from.push(i);
    folded += f;
    i += ch.length;
  }
  from.push(text.length);
  const marked = new Array<boolean>(text.length).fill(false);
  for (const word of words) {
    for (let at = folded.indexOf(word); at >= 0; at = folded.indexOf(word, at + 1)) {
      // Through the end of the character the match ends in.
      const last = from[at + word.length - 1];
      let k = at + word.length;
      while (from[k] === last) k++;
      for (let i = from[at]; i < from[k]; i++) marked[i] = true;
    }
  }
  const parts: Array<{ text: string; match: boolean }> = [];
  for (let i = 0; i < text.length; i++) {
    const last = parts[parts.length - 1];
    if (last && last.match === marked[i]) last.text += text[i];
    else parts.push({ text: text[i], match: marked[i] });
  }
  return parts;
}

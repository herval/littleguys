// The sidebar's other ways in: every guy's chats in one list, grouped by what they're doing,
// the folder they work in (coder mode), or folders you make. Grouped by agent, the sidebar is
// the guy cards (main/GuyList.tsx) and none of this runs.
import { threadId } from "../stores/chat";
import { allGuys, gatewayOf, hasMainChat, sessionsFor, threadKind } from "./guys";
import type { Guy, Roster, SessionRow } from "./types";

export type GroupBy = "agent" | "state" | "workspace" | "folder";
export const GROUP_BYS: GroupBy[] = ["agent", "state", "workspace", "folder"];

export type ListedState = "needs-you" | "working" | "unread" | "idle";

export interface ListedThread {
  guy: Guy;
  row: SessionRow;
  /** `gateway|sessionKey` (stores/chat's `threadId`). */
  id: string;
  state: ListedState;
}

export interface ThreadGroup {
  /** Stable from one render to the next: whether it's closed is kept by it. */
  id: string;
  title: string;
  threads: ListedThread[];
  /** A folder you made: it can be renamed, deleted, and dropped on. */
  folderId?: string;
  /** "Everything else": dropping a thread here takes it out of its folder. */
  rest?: boolean;
  /** On hover: a workspace's full path. */
  detail?: string;
}

export interface Folder {
  id: string;
  name: string;
}

/** Pinned first, then the newest. */
function byActivity(a: ListedThread, b: ListedThread): number {
  return Number(Boolean(b.row.pinned)) - Number(Boolean(a.row.pinned)) || (b.row.updatedAt ?? 0) - (a.row.updatedAt ?? 0);
}

/** Every guy's chats (not routine runs, helpers or archived ones), pinned first, then the newest. */
export function listThreads(roster: Roster): ListedThread[] {
  const threads: ListedThread[] = [];
  for (const guy of allGuys(roster)) {
    const g = gatewayOf(roster, guy.gatewayId);
    if (!g) continue;
    // An approval that doesn't name its thread goes on the guy's main chat.
    const waiting = new Set(
      g.approvals.map((a) => a.sessionKey ?? a.request?.sessionKey ?? `agent:${a.agentId ?? a.request?.agentId}:main`),
    );
    const rows = sessionsFor(roster, guy).filter((r) => !r.archived && ["main", "chat", "channel"].includes(threadKind(r.key)));
    // A guy you haven't talked to yet still shows up, as their main chat.
    const main = `agent:${guy.id}:main`;
    if (hasMainChat(roster, guy.gatewayId) && !rows.some((r) => r.key === main)) {
      rows.push({ key: main, kind: "direct", updatedAt: null } as SessionRow);
    }
    for (const row of rows) {
      const state: ListedState = waiting.has(row.key)
        ? "needs-you"
        : g.running.includes(row.key)
          ? "working"
          : row.unread
            ? "unread"
            : "idle";
      threads.push({ guy, row, id: threadId(guy.gatewayId, row.key), state });
    }
  }
  return threads.sort(byActivity);
}

const STATES: Array<[ListedState, string]> = [
  ["needs-you", "Needs your OK"],
  ["working", "Working"],
  ["unread", "Unread"],
  ["idle", "Idle"],
];

/** Waiting on you, then working, unread and the rest; only the states something is in. */
export function groupByState(threads: ListedThread[]): ThreadGroup[] {
  return STATES.map(([state, title]) => ({ id: state, title, threads: threads.filter((t) => t.state === state) })).filter(
    (g) => g.threads.length > 0,
  );
}

/** What a thread's repo status (lib/code.ts) says about where it works. */
export interface RepoWhere {
  repo: { root: string; name: string; mainRoot: string | null };
}

/** The thread's folder, as its runtime gives it (Claude Code, Codex and ACP agents do). */
export function workspaceOf(t: ListedThread): string | null {
  const dir = t.row.workspaceDir?.replace(/[\\/]+$/, "");
  return dir || null;
}

function lastPart(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

/**
 * By the repo each thread works in (a worktree goes under the repo it came from), else by its
 * folder; threads with neither go last, under "Other chats". `statusOf` answers a thread's repo
 * status, null outside a repo, undefined while unknown. One thread's status speaks for every
 * thread in the same folder, so a folder only needs one read.
 */
export function groupByWorkspace(threads: ListedThread[], statusOf: (id: string) => RepoWhere | null | undefined): ThreadGroup[] {
  const repoOfDir = new Map<string, RepoWhere>();
  for (const t of threads) {
    const status = statusOf(t.id);
    const dir = workspaceOf(t);
    if (status && dir) repoOfDir.set(dir, status);
  }
  const groups = new Map<string, ThreadGroup>();
  const other: ListedThread[] = [];
  for (const t of threads) {
    const dir = workspaceOf(t);
    const status = statusOf(t.id) || (dir ? repoOfDir.get(dir) : undefined);
    const root = status ? (status.repo.mainRoot ?? status.repo.root) : dir;
    if (!root) {
      other.push(t);
      continue;
    }
    const id = `ws:${root}`;
    const group = groups.get(id) ?? { id, title: status?.repo.name ?? lastPart(root), detail: root, threads: [] };
    group.threads.push(t);
    groups.set(id, group);
  }
  // Threads come newest first, so each workspace sits where its newest thread would.
  const list = [...groups.values()];
  if (other.length) list.push({ id: "ws:other", title: "Other chats", threads: other });
  return list;
}

/** Your folders, in the order you made them (empty ones too), then everything not in one. */
export function groupByFolder(threads: ListedThread[], folders: Folder[], filed: Record<string, string>): ThreadGroup[] {
  const groups: ThreadGroup[] = folders.map((f) => ({ id: `folder:${f.id}`, folderId: f.id, title: f.name, threads: [] }));
  const byFolder = new Map(groups.map((g) => [g.folderId, g]));
  const rest: ListedThread[] = [];
  for (const t of threads) {
    const group = byFolder.get(filed[t.id]);
    if (group) group.threads.push(t);
    else rest.push(t);
  }
  return [...groups, { id: "folder:rest", title: "Everything else", threads: rest, rest: true }];
}
